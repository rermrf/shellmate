pub(crate) mod commands;
pub mod exec;
pub mod host;
pub(crate) mod markdown;
mod scroll;
pub mod model;
pub mod panel;
pub mod settings;
pub(crate) mod tasks;
use crate::dialogs::{self, DialogBody, Tone};
use gpui::{prelude::*, *};
use shellmate_ai::{AiError, Config};
use shellmate_store::{CredentialStore, SharedStore};
use std::{cell::RefCell, rc::Rc, sync::Arc};
pub fn authorize(
    db: SharedStore,
    credentials: Arc<CredentialStore>,
    window: &mut Window,
    cx: &mut App,
    done: impl FnOnce(Result<Config, AiError>, &mut Window, &mut App) + 'static,
) {
    let stored = { db.borrow().ai_settings() };
    let settings = match stored {
        Ok(s) if s.configured() => s,
        _ => {
            window.defer(cx, move |w, cx| done(Err(AiError::NotConfigured), w, cx));
            return;
        }
    };
    authorize_settings(db, credentials, settings, window, cx, done);
}
/// Authorize an unsaved form against its actual destination without persisting it.
pub fn authorize_settings(
    db: SharedStore,
    credentials: Arc<CredentialStore>,
    settings: shellmate_store::ai::Settings,
    window: &mut Window,
    cx: &mut App,
    done: impl FnOnce(Result<Config, AiError>, &mut Window, &mut App) + 'static,
) {
    let manager = credentials.clone();
    crate::security::ensure_unlocked(
        credentials,
        "使用 AI 助手",
        window,
        cx,
        move |ok, w, cx| {
            if !ok {
                done(Err(AiError::Cancelled), w, cx);
                return;
            }
            let task = cx
                .background_executor()
                .spawn(async move { model::authorized_config(&settings, manager.as_ref(), true) });
            let handle = w.window_handle();
            cx.spawn(async move |cx| {
                let config = task.await;
                cx.update_window(handle, |_, w, cx| match config {
                    Err(e) => done(Err(e), w, cx),
                    Ok(config) => {
                        let destination = config.provider.destination().expect("validated config");
                        let accepted = db.borrow().ai_disclosed(&destination).unwrap_or(false);
                        if !model::needs_disclosure(
                            &destination,
                            &if accepted {
                                vec![destination.clone()]
                            } else {
                                vec![]
                            },
                        ) {
                            done(Ok(config), w, cx);
                            return;
                        }
                        let completion: DisclosureDone =
                            Rc::new(RefCell::new(Some(Box::new(move |accepted, w, cx| {
                                if accepted {
                                    done(Ok(config), w, cx)
                                } else {
                                    done(Err(AiError::Cancelled), w, cx)
                                }
                            }))));
                        let body = cx.new(|_| Disclosure {
                            db,
                            destination,
                            completion,
                            error: None,
                        });
                        dialogs::open(w, cx, "发送前确认", 500., body);
                    }
                })
                .ok();
            })
            .detach();
        },
    );
}
type DisclosureDone = Rc<RefCell<Option<Box<dyn FnOnce(bool, &mut Window, &mut App)>>>>;
struct Disclosure {
    db: SharedStore,
    destination: String,
    completion: DisclosureDone,
    error: Option<String>,
}
impl DialogBody for Disclosure {
    fn confirm(&mut self, w: &mut Window, cx: &mut Context<Self>) -> bool {
        if self
            .db
            .borrow_mut()
            .accept_ai_disclosure(&self.destination)
            .is_err()
        {
            self.error = Some("无法保存授权，请重试".into());
            cx.notify();
            return false;
        }
        if let Some(done) = self.completion.borrow_mut().take() {
            w.defer(cx, move |w, cx| done(true, w, cx));
        }
        true
    }
    fn cancel(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(done) = self.completion.borrow_mut().take() {
            w.defer(cx, move |w, cx| done(false, w, cx));
        }
    }
}
impl Render for Disclosure {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = dialogs::palette(cx);
        dialogs::stack().child(dialogs::hint(format!("将把最近 40 行终端显示文本、你的问题、本次对话的历史，以及 AI 执行的命令和输出原样发送到 {}。Shellmate 保存的登录凭据不会发送。首次同意后，会记住此地址；测试连接也会向该地址发送一句测试问题。",self.destination),&p)).children(self.error.clone().map(|e|dialogs::notice(dialogs::Notice::Error,e,&p))).child(dialogs::actions(vec![dialogs::button("ai-disclosure-cancel","取消",Tone::Plain,true).on_click(cx.listener(|s,_,w,cx|s.dismiss(w,cx))),dialogs::button("ai-disclosure-ok","同意并继续",Tone::Primary,true).on_click(cx.listener(|s,_,w,cx|s.submit(w,cx)))]))
    }
}
impl Drop for Disclosure {
    fn drop(&mut self) {
        self.completion.borrow_mut().take();
    }
}
pub fn now() -> i64 {
    #[cfg(debug_assertions)]
    if crate::proto::requested() {
        return crate::proto::now();
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
