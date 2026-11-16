//! The host editor: create, edit and copy saved hosts.

mod protocols;

use crate::connect::{self, Answer, FormCredential, UiMsg};
use gpui::assets::IconName as Lucide;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputEvent, InputState};
use gpui::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui::component::{Icon, WindowExt as _};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, Focusable as _, FontWeight, IntoElement, PathPromptOptions, Render, SharedString,
    ScrollHandle, Subscription, WeakEntity, Window, div, px,
};
use shellmate_core::{AuthMethod, Host, HostBook, HostId, Protocol};
use shellmate_ssh::KeyInfo;

use super::{DialogBody, Notice, Tone, button, hint, notice, palette, stack};
use crate::library::{EditorFields, build_draft};
use crate::theme::Palette;
use crate::ui::{SegmentItem, segmented};
use crate::workspace::Workspace;

/// How the editor starts.
pub enum EditorMode {
    /// An empty form.
    New,
    /// Editing a saved host.
    Edit(Host),
    /// A new host prefilled from a saved one.
    Duplicate(Host),
}

/// Authentication methods in the order of the prototype's selector.
const AUTH_ORDER: [AuthMethod; 4] = [
    AuthMethod::Password,
    AuthMethod::PrivateKey,
    AuthMethod::Agent,
    AuthMethod::KeyboardInteractive,
];

// Prototype shell: title row 66, protocol track 38 + bottom gap 16,
// footer 1 px hairline + 14 px inset + 40 px buttons + 20 px inset.
const HEADER_HEIGHT: f32 = 66.;
const PROTOCOL_HEIGHT: f32 = 54.;
const FOOTER_HEIGHT: f32 = 75.;

fn editor_height(viewport: f32, protocol: Protocol, advanced: bool) -> f32 {
    let preferred = match protocol {
        Protocol::Serial if advanced => 650.,
        Protocol::Serial => 460.,
        Protocol::Telnet => 620.,
        Protocol::Ssh | Protocol::Mosh => 840.,
    };
    super::shell_height(viewport, preferred, 30.)
}

fn focus_scroll_delta(top: f32, bottom: f32, field_top: f32, field_bottom: f32) -> f32 {
    if field_top < top + 4. { top + 4. - field_top }
    else if field_bottom > bottom - 4. { bottom - 4. - field_bottom }
    else { 0. }
}

/// The form for one host.
pub struct HostEditor {
    workspace: WeakEntity<Workspace>,
    /// The saved host being edited (`None` for new hosts and copies).
    original: Option<Host>,
    protocol: Protocol,
    serial: shellmate_core::SerialSettings,
    baud: Entity<InputState>,
    devices: Vec<shellmate_serial::Device>,
    advanced: bool,
    form_scroll: ScrollHandle,
    name: Entity<InputState>,
    addr: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    tags: Entity<InputState>,
    group: Entity<InputState>,
    identity: Entity<InputState>,
    password: Entity<InputState>,
    auth: AuthMethod,
    base: Host,
    passphrase: Entity<InputState>,
    key_info: Option<Result<KeyInfo, String>>,
    key_task: Option<gpui::Task<()>>,
    save_secret: bool,
    test_abort: Option<tokio::task::AbortHandle>,
    test_tasks: Vec<gpui::Task<()>>,
    test_result: Option<(bool, String)>,
    jumps: Vec<HostId>,
    /// Every other SSH host: the jump host choices.
    choices: Vec<(HostId, String)>,
    groups: Vec<String>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl HostEditor {
    /// Opens the editor. Saving goes through `workspace`.
    pub fn open(
        workspace: WeakEntity<Workspace>,
        mode: EditorMode,
        book: &HostBook,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (original, source) = match mode {
            EditorMode::New => (None, None),
            EditorMode::Edit(host) => (Some(host.clone()), Some(host)),
            EditorMode::Duplicate(mut host) => {
                host.name = format!("{}-copy", host.name);
                (None, Some(host))
            }
        };
        let editing_id = original.as_ref().map(|h| h.id.clone());
        let choices = book
            .hosts
            .iter()
            .filter(|h| h.protocol == Protocol::Ssh && Some(&h.id) != editing_id.as_ref())
            .map(|h| (h.id.clone(), h.name.clone()))
            .collect();
        let groups = book.groups.iter().map(|g| g.name.clone()).collect();
        let group_name = source
            .as_ref()
            .and_then(|h| h.group.as_ref())
            .and_then(|id| book.group(id))
            .map(|g| g.name.clone())
            .unwrap_or_default();

        let group_name = if crate::prototype_fixture() && group_name.is_empty() {
            "生产环境".to_string()
        } else {
            group_name
        };
        let body = cx.new(|cx| {
            let mut text = |value: String, placeholder: &'static str, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(value)
                        .placeholder(placeholder)
                })
            };
            let source = source.as_ref();
            let name = text(
                source.map(|h| h.name.clone()).unwrap_or_default(),
                "prod-web-03",
                cx,
            );
            let addr = text(
                source.map(|h| h.addr.clone()).unwrap_or_default(),
                "10.0.1.23 或 example.com",
                cx,
            );
            let port = text(source.map_or(22, |h| h.port).to_string(), "22", cx);
            let user = text(
                source.and_then(|h| h.user.clone()).unwrap_or_else(|| {
                    if crate::prototype_fixture() {
                        "root".into()
                    } else {
                        String::new()
                    }
                }),
                "留空使用本机用户名",
                cx,
            );
            let tags = text(
                source.map(|h| h.tags.join(", ")).unwrap_or_default(),
                "nginx, k8s",
                cx,
            );
            let group = text(group_name, "不分组", cx);
            let identity = text(
                source
                    .and_then(|h| h.identity_file.as_ref())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| {
                        if crate::prototype_fixture() {
                            "~/.ssh/id_ed25519".into()
                        } else {
                            String::new()
                        }
                    }),
                "留空使用 ~/.ssh 里的默认私钥",
                cx,
            );
            let baud = text(
                source.map_or(115200, |h| h.serial.baud).to_string(),
                "自定义波特率",
                cx,
            );
            let password = cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("留空表示不修改")
            });

            let passphrase = cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("输入私钥口令")
            });
            if crate::prototype_fixture() {
                passphrase.update(cx, |s, cx| s.set_value("fixture-only", window, cx));
            }
            let mut subscriptions = Vec::new();
            for input in [&name, &addr, &port, &user, &tags, &group, &identity, &password, &passphrase, &baud] {
                let input = input.clone();
                let focus = input.focus_handle(cx);
                subscriptions.push(cx.on_focus(&focus, window, move |_: &mut Self, window, cx| {
                    let editor = cx.entity().downgrade();
                    let input = input.clone();
                    // Read geometry after layout, including newly expanded advanced fields.
                    window.on_next_frame(move |window, cx| {
                        if !input.focus_handle(cx).is_focused(window) {return;}
                        editor.update(cx, |this, cx| {
                            let viewport = this.form_scroll.bounds();
                            let field = input.read(cx).input_bounds();
                            if viewport.size.height <= px(0.) || field.size.height <= px(0.) { return; }
                            let delta = focus_scroll_delta(f32::from(viewport.top()), f32::from(viewport.bottom()), f32::from(field.top()), f32::from(field.bottom()));
                            if delta != 0. {
                                let mut offset = this.form_scroll.offset();
                                offset.y = (offset.y + px(delta)).clamp(-this.form_scroll.max_offset().y, px(0.));
                                this.form_scroll.set_offset(offset);
                                cx.notify();
                            }
                        }).ok();
                    });
                }));
            }
            for input in [
                &name,
                &addr,
                &port,
                &user,
                &tags,
                &group,
                &password,
                &passphrase,
                &baud,
            ] {
                subscriptions.push(cx.subscribe_in(
                    input,
                    window,
                    |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                        InputEvent::PressEnter { .. } => this.submit(window, cx),
                        InputEvent::Change => {
                            this.invalidate_test();
                            this.error = None;
                            cx.notify();
                        }
                        _ => {}
                    },
                ));
            }
            subscriptions.push(cx.subscribe_in(
                &identity,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        this.invalidate_test();
                        this.inspect_key(window, cx);
                    }
                    InputEvent::PressEnter { .. } => this.submit(window, cx),
                    _ => {}
                },
            ));
            Self {
                base: original.clone().unwrap_or_else(|| Host::new("", "")),
                passphrase,
                key_info: None,
                key_task: None,
                save_secret: true,
                test_abort: None,
                test_tasks: Vec::new(),
                test_result: None,
                workspace,
                protocol: source.map_or(Protocol::Ssh, |h| h.protocol),
                serial: source.map(|h| h.serial.clone()).unwrap_or_default(),
                baud,
                devices: vec![],
                advanced: false,
                form_scroll: ScrollHandle::new(),
                original,
                name,
                addr,
                port,
                user,
                tags,
                group,
                identity,
                password,
                auth: source.map_or(AuthMethod::default(), |h| if h.protocol==Protocol::Telnet {AuthMethod::Password}else{h.auth}),
                jumps: source.map(|h| h.jump.clone()).unwrap_or_default(),
                choices,
                groups,
                error: None,
                _subscriptions: subscriptions,
            }
        });
        #[cfg(debug_assertions)]
        body.update(cx, |body, cx| {
            let scene = std::env::var("SHELLMATE_DEMO").unwrap_or_default();
            if scene.starts_with("proto:host-serial") {
                body.change_protocol(Protocol::Serial, window, cx);
                body.set_text(&body.addr, "/dev/cu.usbserial-0001", window, cx);
            } else if scene.starts_with("proto:host-mosh") {
                body.change_protocol(Protocol::Mosh,window,cx);
            } else if scene.starts_with("proto:host-telnet") {
                body.change_protocol(Protocol::Telnet, window, cx);
            }
        });
        body.update(cx, |body, cx| {
            body.inspect_key(window, cx);
            if body.protocol == Protocol::Serial {
                body.scan_devices(window, cx);
            }
        });
        let first = std::cell::Cell::new(true);
        crate::dialogs::open_modal(window, cx, move |dialog, window, cx| {
            if first.replace(false) && !crate::prototype_fixture() {
                body.update(cx, |body, cx| body.focus(window, cx));
            }
            let confirm = body.clone();
            let cancel = body.clone();
            let editor = body.read(cx);
            let height = px(editor_height(f32::from(window.viewport_size().height), editor.protocol, editor.advanced));
            dialog
                .w(px(660.))
                .h(height)
                .margin_top(((window.viewport_size().height - height) / 2.).max(px(30.)))
                .rounded(px(26.))
                .p(px(0.))
                .gap(px(0.))
                .border_0()
                .shadow(super::modal_shadow(None))
                .close_button(false)
                .overlay_closable(false)
                .on_ok(move |_, w, cx| confirm.update(cx, |b, cx| b.confirm(w, cx)))
                .on_cancel(move |_, w, cx| {
                    cancel.update(cx, |b, cx| b.cancel(w, cx));
                    true
                })
                .map(|d| super::fixed_shell(d, body.clone(), height))
        });
    }

    fn fields(&self, cx: &App) -> EditorFields {
        let value = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let mut serial = self.serial.clone();
        serial.baud = value(&self.baud).parse().unwrap_or(0);
        EditorFields {
            protocol: self.protocol,
            serial,
            name: value(&self.name),
            addr: value(&self.addr),
            port: value(&self.port),
            user: value(&self.user),
            tags: value(&self.tags),
            group: value(&self.group),
            identity_file: value(&self.identity),
            password: value(&self.password),
            auth: self.auth,
            jumps: self.jumps.clone(),
        }
    }

    fn set_text(
        &self,
        input: &Entity<InputState>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.to_string();
        input.update(cx, |input, cx| input.set_value(text, window, cx));
    }

    fn choose_key_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择私钥".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await
                && let Some(path) = paths.first()
            {
                let text = path.display().to_string();
                this.update_in(cx, |this, window, cx| {
                    this.set_text(&this.identity.clone(), &text, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    fn move_jump(&mut self, index: usize, delta: isize, cx: &mut Context<Self>) {
        let target = index as isize + delta;
        if target >= 0 && (target as usize) < self.jumps.len() {
            self.jumps.swap(index, target as usize);
            self.error = None;
            cx.notify();
        }
    }

    fn name_of(&self, id: &HostId) -> String {
        self.choices
            .iter()
            .find(|(c, _)| c == id)
            .map_or_else(|| "（已删除的主机）".to_string(), |(_, n)| n.clone())
    }

    fn invalidate_test(&mut self) {
        if let Some(abort) = self.test_abort.take() {
            abort.abort();
        }
        self.test_tasks.clear();
        self.test_result = None;
    }

    fn inspect_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.key_task = None;
        self.key_info = None;
        let text = self.identity.read(cx).value().to_string();
        if text.trim().is_empty() {
            cx.notify();
            return;
        }
        if crate::prototype_fixture() {
            // Fixture metadata never opens the displayed ~/.ssh path.
            self.key_info = Some(Ok(KeyInfo {
                algorithm: "ED25519".into(),
                encrypted: true,
                cipher: "aes256-ctr".into(),
                kdf: "bcrypt-pbkdf".into(),
            }));
            cx.notify();
            return;
        }
        let path = crate::library::expand_tilde(text.trim(), std::env::home_dir().as_deref());
        let task = cx
            .background_executor()
            .spawn(async move { KeyInfo::read(&path) });
        self.key_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.key_info = Some(result);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn credential(&self, host: &Host, cx: &App) -> Option<FormCredential> {
        if self.protocol==Protocol::Serial {return None;}
        let value = match self.auth {
            AuthMethod::Password => self.password.read(cx).value(),
            AuthMethod::PrivateKey => self.passphrase.read(cx).value(),
            _ => return None,
        };
        if value.is_empty() {
            return None;
        }
        Some(FormCredential {
            method: self.auth,
            key: host.identity_file.clone(),
            answer: Answer {
                secret: shellmate_core::Secret::new(value.as_str()),
                save: self.save_secret,
            },
        })
    }

    fn test_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.invalidate_test();
        if crate::prototype_fixture() {
            self.test_result = Some((false, "原型预览不发起网络连接".into()));
            cx.notify();
            return;
        }
        let draft = match build_draft(
            &self.fields(cx),
            Some(&self.base),
            std::env::home_dir().as_deref(),
        ) {
            Ok(draft) => draft,
            Err(e) => {
                self.error = Some(e.into());
                cx.notify();
                return;
            }
        };
        if matches!(self.protocol, Protocol::Telnet | Protocol::Serial) {
            self.test_stream(draft.host, window, cx);
            return;
        }
        let setup = self.workspace.update(cx, |workspace, _| {
            workspace
                .editor_route(&draft.host)
                .map(|chain| (chain, workspace.editor_secrets()))
        });
        let (chain, secrets) = match setup {
            Ok(Ok(setup)) => setup,
            Ok(Err(e)) => {
                self.error = Some(e.into());
                cx.notify();
                return;
            }
            Err(_) => return,
        };
        let Some(runtime) = crate::runtime::get(cx) else {
            self.error = Some("SSH 运行时未能启动".into());
            cx.notify();
            return;
        };
        let (tx, rx) = async_channel::unbounded();
        let bridge = connect::UiBridge::new(
            tx,
            &chain,
            &connect::default_user(),
            connect::AuthMemory::new(),
        );
        if let Some(credential) = self.credential(&draft.host, cx) {
            bridge.supply_form(
                connect::Endpoint::of(&draft.host, &connect::default_user()),
                credential,
            );
        }
        let join = runtime.spawn(connect::test_connection(chain, secrets, bridge));
        self.test_abort = Some(join.abort_handle());
        let pump = cx.spawn_in(window, async move |this, cx| {
            while let Ok(message) = rx.recv().await {
                if this
                    .update_in(cx, |_, window, cx| match message {
                        UiMsg::HostKey { request, reply } => {
                            super::hostkey::HostKeyDialog::open(request, reply, window, cx)
                        }
                        UiMsg::Password { request, reply } => {
                            super::auth::SecretDialog::open_password(request, reply, window, cx)
                        }
                        UiMsg::Passphrase { request, reply } => {
                            super::auth::SecretDialog::open_passphrase(request, reply, window, cx)
                        }
                        UiMsg::Challenge { request, reply } => {
                            super::auth::ChallengeDialog::open(request, reply, window, cx)
                        }
                        UiMsg::Event(_) => {}
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let result = cx.spawn_in(window, async move |this, cx| {
            let outcome = join.await;
            this.update(cx, |this, cx| {
                this.test_abort = None;
                this.test_result = Some(match outcome {
                    Ok(Ok((method, saves))) => (
                        true,
                        format!(
                            "连接成功 · 认证方式 {}{}",
                            method.map_or("无需凭据", AuthMethod::label),
                            if saves.errors.is_empty() {
                                String::new()
                            } else {
                                format!(" · 钥匙串：{}", saves.errors.join("；"))
                            }
                        ),
                    ),
                    Ok(Err(e)) => (false, e.error.to_string()),
                    Err(e) => (false, format!("连接测试已中止：{e}")),
                });
                cx.notify();
            })
            .ok();
        });
        self.test_tasks = vec![pump, result];
        self.error = None;
        cx.notify();
    }

    fn input(&self, state: &Entity<InputState>, p: &Palette, mono: bool) -> Input {
        Input::new(state)
            .flex_none()
            .bordered(false)
            .focus_bordered(false)
            .bg(p.card2)
            // Input::h is a multiline option; constrain the single-line control's actual box.
            .min_h(px(38.))
            .max_h(px(38.))
            .rounded(px(11.))
            .px(px(12.))
            .text_size(px(13.))
            .line_height(px(18.))
            .when(mono, |i| i.font_family(crate::theme::MONO_FONT))
    }

    fn field(&self, label: &'static str, p: &Palette, control: impl IntoElement) -> gpui::Div {
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .h(px(15.))
                    .text_size(px(12.))
                    .line_height(px(15.))
                    .text_color(p.tx2)
                    .child(label),
            )
            .child(control)
    }

    fn render_auth_details(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::Div {
        match self.auth {
            AuthMethod::Password => stack().gap(px(10.)).child(self.field(
                "密码",
                p,
                self.input(&self.password, p, false).mask_toggle(),
            )),
            AuthMethod::PrivateKey => div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(self.input(&self.identity, p, true)),
                        )
                        .child(
                            button("pick-key", "选择…", Tone::Plain, true)
                                .h(px(38.))
                                .w(px(64.))
                                .on_click(
                                    cx.listener(|this, _, w, cx| this.choose_key_file(w, cx)),
                                ),
                        ),
                )
                .children(self.key_info.as_ref().map(|info| {
                    match info {
                        Ok(info) => div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(19.))
                            .child(
                                div()
                                    .px(px(7.))
                                    .h(px(19.))
                                    .text_size(px(12.))
                                    .line_height(px(19.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .rounded(px(7.))
                                    .bg(p.accs)
                                    .text_color(p.acc)
                                    .child(info.algorithm.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .line_height(px(15.))
                                    .text_color(p.tx2)
                                    .child(info.description()),
                            ),
                        Err(error) => div()
                            .text_size(px(12.))
                            .line_height(px(17.))
                            .text_color(p.err)
                            .child(error.clone()),
                    }
                }))
                .when(
                    self.key_info
                        .as_ref()
                        .is_some_and(|i| i.as_ref().is_ok_and(|i| i.encrypted)),
                    |d| {
                        d.child(self.field(
                            "私钥口令",
                            p,
                            self.input(&self.passphrase, p, false).mask_toggle(),
                        ))
                    },
                ),
            AuthMethod::Agent => stack().child(hint(
                "使用 SSH_AUTH_SOCK 指向的 ssh-agent 里的密钥（如 1Password、macOS 钥匙串代理）。",
                p,
            )),
            AuthMethod::KeyboardInteractive => stack().child(hint(
                "服务端会依次发起提示（如密码 → 动态验证码）；验证码每次手动输入。",
                p,
            )),
        }
    }

    fn render_jumps(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::Div {
        let free: Vec<_> = self
            .choices
            .iter()
            .filter(|(id, _)| !self.jumps.contains(id))
            .cloned()
            .collect();
        let this = cx.entity().downgrade();
        let add = button("add-jump", "添加一跳", Tone::Plain, !free.is_empty())
            .text_color(p.acc)
            .border_1()
            .border_dashed()
            .border_color(p.line)
            .dropdown_menu(move |mut menu, _, _| {
                for (id, name) in &free {
                    let id = id.clone();
                    let this = this.clone();
                    menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.jumps.push(id.clone());
                            this.invalidate_test();
                            cx.notify();
                        })
                        .ok();
                    }));
                }
                menu
            });
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(10.))
            .mt(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .line_height(px(20.))
                            .font_weight(FontWeight::BOLD)
                            .child("跳板机 ProxyJump"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(17.))
                            .text_color(p.tx2)
                            .child("按顺序逐跳连接，每跳独立校验指纹"),
                    ),
            )
            .child(div().flex().child(add))
            .child(
                div()
                    .min_h(px(37.))
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(11.))
                    .bg(p.card2)
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.tx2)
                    .child("本机")
                    .children(self.jumps.iter().enumerate().map(|(ix, id)| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .child("→")
                            .child(self.name_of(id))
                            .child(
                                Button::new(("jump-up", ix))
                                    .ghost()
                                    .compact()
                                    .icon(Icon::new(Lucide::ArrowLeft).size(px(12.)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.invalidate_test();
                                        this.move_jump(ix, -1, cx);
                                    })),
                            )
                            .child(
                                Button::new(("jump-down", ix))
                                    .ghost()
                                    .compact()
                                    .icon(Icon::new(Lucide::ArrowRight).size(px(12.)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.invalidate_test();
                                        this.move_jump(ix, 1, cx);
                                    })),
                            )
                            .child(
                                Button::new(("jump-remove", ix))
                                    .ghost()
                                    .compact()
                                    .icon(Icon::new(Lucide::X).size(px(12.)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.jumps.remove(ix);
                                        this.invalidate_test();
                                        cx.notify();
                                    })),
                            )
                    }))
                    .child("→")
                    .child("目标主机"),
            )
    }
}

impl DialogBody for HostEditor {
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let home = std::env::home_dir();
        let draft = match build_draft(&self.fields(cx), Some(&self.base), home.as_deref()) {
            Ok(draft) => draft,
            Err(message) => {
                self.error = Some(message.into());
                cx.notify();
                return false;
            }
        };
        if self.test_abort.is_some() {
            return false;
        }
        let credential = self.credential(&draft.host, cx);
        let saved = self.workspace.update(cx, |workspace, cx| {
            workspace.save_host_draft(draft, credential, window, cx)
        });
        match saved {
            Ok(Ok(())) => true,
            Ok(Err(message)) => {
                self.error = Some(message.into());
                cx.notify();
                false
            }
            Err(_) => true,
        }
    }

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.invalidate_test();
    }

    fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.name.update(cx, |input, cx| input.focus(window, cx));
    }
}

impl Drop for HostEditor {
    fn drop(&mut self) {
        if let Some(abort) = self.test_abort.take() {
            abort.abort();
        }
    }
}

impl Render for HostEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let this = cx.entity().downgrade();
        let protocols = [
            Protocol::Ssh,
            Protocol::Mosh,
            Protocol::Telnet,
            Protocol::Serial,
        ]
        .iter()
        .map(|protocol| SegmentItem::new(protocol.label(), *protocol == self.protocol))
        .collect();
        let auth_picker = segmented(
            "editor-auth",
            AUTH_ORDER
                .iter()
                .map(|a| SegmentItem::new(a.label(), *a == self.auth))
                .collect(),
            move |index, _, cx| {
                this.update(cx, |this, cx| {
                    this.auth = AUTH_ORDER[index];
                    this.invalidate_test();
                    this.error = None;
                    cx.notify();
                })
                .ok();
            },
            cx,
        );
        let this = cx.entity().downgrade();
        let groups = self.groups.clone();
        let group_picker = Button::new("group-list")
            .ghost()
            .compact()
            .icon(Icon::new(Lucide::ChevronDown).size(px(14.)))
            .dropdown_menu(move |mut menu, _, _| {
                for name in &groups {
                    let this = this.clone();
                    let name = name.clone();
                    menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, w, cx| {
                        this.update(cx, |this, cx| {
                            this.set_text(&this.group.clone(), &name, w, cx)
                        })
                        .ok();
                    }));
                }
                menu
            });
        let this = cx.entity().downgrade();
        let toggle = crate::ui::Toggle::new("editor-keychain")
            .checked(self.save_secret)
            .on_click(move |on, _, cx| {
                this.update(cx, |this, cx| {
                    this.save_secret = *on;
                    this.invalidate_test();
                    cx.notify();
                })
                .ok();
            });
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .text_color(p.tx)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(24.))
                    .pt(px(22.))
                    .pb(px(14.))
                    .h(px(HEADER_HEIGHT))
                    .flex_none()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(22.))
                            .line_height(px(26.))
                            .font_weight(FontWeight::BOLD)
                            .child(if self.original.is_some() { "编辑连接" } else { "新建连接" }),
                    )
                    .child(
                        button("editor-import", "从 ~/.ssh/config 导入", Tone::Plain, true)
                            .h(px(30.))
                            .w(px(148.))
                            .bg(p.accs)
                            .text_color(p.acc)
                            .text_size(px(12.5))
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.invalidate_test();
                                w.close_dialog(cx);
                                this.workspace.update(cx, |workspace, cx| workspace.open_import(w, cx)).ok();
                            })),
                    )
                    .child(
                        button("editor-close", "×", Tone::Plain, true)
                            .h(px(30.))
                            .w(px(30.))
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    ),
            )
            .child(div().h(px(PROTOCOL_HEIGHT)).px(px(24.)).pb(px(16.)).flex_none().child(segmented(
                "editor-protocol",
                protocols,
                {let weak=cx.entity().downgrade();move |ix,w,cx|{weak.update(cx,|this,cx|this.change_protocol([Protocol::Ssh,Protocol::Mosh,Protocol::Telnet,Protocol::Serial][ix],w,cx)).ok();}},
                cx,
            )))
            .child(
                div()
                    .id("editor-fields")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.form_scroll)
                    .mx(px(24.))
                    .pr(px(0.))
                    .pb(px(20.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .when(self.protocol!=Protocol::Serial,|d| d
                    .child(
                        div().flex_none()
                            .flex()
                            .gap(px(14.))
                            .child(div().flex_1().min_w_0().child(self.field(
                                "名称",
                                &p,
                                self.input(&self.name, &p, false),
                            )))
                            .child(div().flex_1().min_w_0().child(self.field(
                                "分组",
                                &p,
                                self.input(&self.group, &p, false).suffix(group_picker),
                            ))),
                    )
                    .child(
                        div().flex_none()
                            .flex()
                            .gap(px(10.))
                            .child(div().flex_1().min_w_0().mr(px(4.)).child(self.field(
                                "主机地址 HostName",
                                &p,
                                self.input(&self.addr, &p, true),
                            )))
                            .child(div().w(px(195.)).child(self.field(
                                "用户 User",
                                &p,
                                self.input(&self.user, &p, true),
                            )))
                            .child(div().w(px(90.)).child(self.field("端口", &p, self.input(&self.port, &p, true)))),
                    )
                    .child(self.field("标签（逗号分隔）", &p, self.input(&self.tags, &p, false)))
                    )
                    .children((self.protocol==Protocol::Serial).then(||self.render_serial_fields(&p,cx)))
                    .when(self.protocol==Protocol::Telnet,|d|d.child(div().flex_none().px(px(14.)).py(px(12.)).rounded(px(14.)).bg(p.warns).text_color(p.warn).text_size(px(12.5)).line_height(px(20.)).font_weight(FontWeight::SEMIBOLD).child("Telnet 以明文传输账号与数据，仅建议用于内网设备。登录密码仍会存入钥匙串。")).child(self.field("密码",&p,self.input(&self.password,&p,false))))
                    .when(matches!(self.protocol,Protocol::Ssh|Protocol::Mosh),|d|d
                    .child(
                        div().flex_none()
                            .mt(px(6.))
                            .flex()
                            .flex_col()
                            .gap(px(12.))
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .line_height(px(20.))
                                    .font_weight(FontWeight::BOLD)
                                    .child("认证方式"),
                            )
                            .child(auth_picker),
                    )
                    .child(self.render_auth_details(&p, cx))
)
                    .when(self.protocol!=Protocol::Serial,|d|d
                    .child(
                        div().flex_none()
                            .px(px(14.))
                            .py(px(12.))
                            .rounded(px(14.))
                            .bg(p.card2)
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .text_size(px(13.))
                                            .line_height(px(18.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("密码与口令存入 macOS 钥匙串"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .line_height(px(17.))
                                            .text_color(p.tx2)
                                            .child("本地配置库中不保存任何明文凭据"),
                                    ),
                            )
                            .child(toggle),
                    )
)
                    .when(matches!(self.protocol,Protocol::Ssh|Protocol::Mosh),|d|d.child(self.render_jumps(&p,cx)))
                    .when(self.protocol==Protocol::Mosh,|d|d.child(div().flex_none().text_size(px(12.5)).line_height(px(20.)).text_color(p.tx2).child("Mosh 先经 SSH 启动 mosh-server（UDP 60000–61000），之后网络漫游、休眠唤醒都无需重连。")))
                    .children(self.error.clone().map(|e| notice(Notice::Error, e, &p)))
                    .children(
                        self.test_result
                            .as_ref()
                            .map(|(ok, text)| notice(if *ok { Notice::Info } else { Notice::Error }, text.clone(), &p)),
                    ),
            )
            .child(
                div()
                    .h(px(FOOTER_HEIGHT))
                    .flex_none()
                    .border_t_1()
                    .border_color(p.line)
                    .px(px(24.))
                    .pt(px(14.))
                    .pb(px(20.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        button(
                            "editor-test",
                            if self.test_abort.is_some() { "连接中…" } else { "测试连接" },
                            Tone::Plain,
                            self.test_abort.is_none(),
                        )
                        .h(px(40.))
                        .rounded(px(13.))
                        .on_click(cx.listener(|this, _, w, cx| this.test_connection(w, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        button("editor-cancel", "取消", Tone::Plain, true)
                            .h(px(40.))
                            .rounded(px(13.))
                            .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                    )
                    .child(
                        button("editor-save", "保存", Tone::Primary, self.test_abort.is_none())
                            .h(px(40.))
                            .w(px(70.))
                            .rounded(px(13.))
                            .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn focus_scroll_moves_only_as_far_as_the_field_needs() {
        assert_eq!(focus_scroll_delta(150., 795., 200., 238.), 0.);
        assert_eq!(focus_scroll_delta(150., 795., 120., 158.), 34.);
        assert_eq!(focus_scroll_delta(150., 595., 610., 648.), -57.);
        assert_eq!(focus_scroll_delta(150., 595., 154., 192.), 0.);
    }

    #[test]
    fn every_protocol_reserves_header_and_footer_inside_the_window() {
        for (protocol, advanced, expected) in [
            (Protocol::Ssh, false, [840., 840., 640.]),
            (Protocol::Mosh, false, [840., 840., 640.]),
            (Protocol::Telnet, false, [620., 620., 620.]),
            (Protocol::Serial, false, [460., 460., 460.]),
            (Protocol::Serial, true, [650., 650., 640.]),
        ] {
            for ((width, viewport), expected) in [(1440., 900.), (1776., 901.), (1100., 700.)].into_iter().zip(expected) {
                let height = editor_height(viewport, protocol, advanced);
                assert_eq!(height, expected, "{protocol:?} at {width}×{viewport}");
                let top = (viewport - height) / 2.;
                let body = height - HEADER_HEIGHT - PROTOCOL_HEIGHT - FOOTER_HEIGHT;
                assert!(top >= 30. && top + height <= viewport - 30.);
                assert!(body >= 265., "form keeps a usable vertical scroll viewport");
                assert_eq!(top + HEADER_HEIGHT + PROTOCOL_HEIGHT + body, viewport - top - FOOTER_HEIGHT);
            }
        }
        // Exact reference: the footer starts at y=795, buttons at y=810.
        assert_eq!(30. + editor_height(900., Protocol::Mosh, false) - FOOTER_HEIGHT, 795.);
    }
}
