//! The host editor: create, edit and copy saved hosts.

use gpui::assets::IconName as Lucide;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputEvent, InputState};
use gpui::component::{Icon, WindowExt as _};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FontWeight, IntoElement, PathPromptOptions, Render, SharedString, Subscription, WeakEntity,
    Window, div, px,
};
use shellmate_core::{AuthMethod, Host, HostBook, HostId, Protocol};

use super::{DialogBody, Notice, Tone, actions, button, field, hint, notice, palette, stack};
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
const AUTH_ORDER: [AuthMethod; 4] =
    [AuthMethod::Password, AuthMethod::PrivateKey, AuthMethod::Agent, AuthMethod::KeyboardInteractive];

/// The form for one host.
pub struct HostEditor {
    workspace: WeakEntity<Workspace>,
    /// The saved host being edited (`None` for new hosts and copies).
    original: Option<Host>,
    protocol: Protocol,
    name: Entity<InputState>,
    addr: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    tags: Entity<InputState>,
    group: Entity<InputState>,
    identity: Entity<InputState>,
    password: Entity<InputState>,
    auth: AuthMethod,
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
        let (title, original, source) = match mode {
            EditorMode::New => ("新建连接", None, None),
            EditorMode::Edit(host) => ("编辑主机", Some(host.clone()), Some(host)),
            EditorMode::Duplicate(mut host) => {
                host.name = format!("{}-copy", host.name);
                ("新建连接", None, Some(host))
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

        let body = cx.new(|cx| {
            let mut text = |value: String, placeholder: &'static str, cx: &mut Context<Self>| {
                cx.new(|cx| InputState::new(window, cx).default_value(value).placeholder(placeholder))
            };
            let source = source.as_ref();
            let name = text(source.map(|h| h.name.clone()).unwrap_or_default(), "prod-web-03", cx);
            let addr = text(source.map(|h| h.addr.clone()).unwrap_or_default(), "10.0.1.23 或 example.com", cx);
            let port = text(source.map_or(22, |h| h.port).to_string(), "22", cx);
            let user = text(source.and_then(|h| h.user.clone()).unwrap_or_default(), "留空使用本机用户名", cx);
            let tags = text(source.map(|h| h.tags.join(", ")).unwrap_or_default(), "nginx, k8s", cx);
            let group = text(group_name, "不分组", cx);
            let identity = text(
                source.and_then(|h| h.identity_file.as_ref()).map(|p| p.display().to_string()).unwrap_or_default(),
                "留空使用 ~/.ssh 里的默认私钥",
                cx,
            );
            let password = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("留空表示不修改"));

            let mut subscriptions = Vec::new();
            for input in [&name, &addr, &port, &user, &tags, &group, &identity, &password] {
                subscriptions.push(cx.subscribe_in(
                    input,
                    window,
                    |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                        InputEvent::PressEnter { .. } => this.submit(window, cx),
                        InputEvent::Change => {
                            this.error = None;
                            cx.notify();
                        }
                        _ => {}
                    },
                ));
            }
            Self {
                workspace,
                protocol: source.map_or(Protocol::Ssh, |h| h.protocol),
                original,
                name,
                addr,
                port,
                user,
                tags,
                group,
                identity,
                password,
                auth: source.map_or(AuthMethod::default(), |h| h.auth),
                jumps: source.map(|h| h.jump.clone()).unwrap_or_default(),
                choices,
                groups,
                error: None,
                _subscriptions: subscriptions,
            }
        });
        super::open(window, cx, title, 580., body);
    }

    fn fields(&self, cx: &App) -> EditorFields {
        let value = |input: &Entity<InputState>| input.read(cx).value().to_string();
        EditorFields {
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

    fn set_text(&self, input: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut Context<Self>) {
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
                this.update_in(cx, |this, window, cx| this.set_text(&this.identity.clone(), &text, window, cx)).ok();
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
        self.choices.iter().find(|(c, _)| c == id).map_or_else(|| "（已删除的主机）".to_string(), |(_, n)| n.clone())
    }

    fn chip(
        &self,
        id: impl Into<gpui::ElementId>,
        label: String,
        active: bool,
        p: &Palette,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .px(px(10.))
            .h(px(26.))
            .flex()
            .items_center()
            .rounded(px(8.))
            .cursor_pointer()
            .text_size(px(12.5))
            .when(active, |d| d.bg(p.accs).text_color(p.acc).font_weight(FontWeight::SEMIBOLD))
            .when(!active, |d| d.bg(p.card2).text_color(p.tx2).hover(|d| d.bg(p.card3).text_color(p.tx)))
            .child(label)
    }

    fn render_auth_details(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::Div {
        match self.auth {
            AuthMethod::Password => stack().child(field("密码", p, Input::new(&self.password).mask_toggle())),
            AuthMethod::PrivateKey => stack().child(field(
                "私钥文件",
                p,
                div()
                    .flex()
                    .gap(px(8.))
                    .child(div().flex_1().child(Input::new(&self.identity)))
                    .child(
                        button("pick-key", "选择…", Tone::Plain, true)
                            .on_click(cx.listener(|this, _, window, cx| this.choose_key_file(window, cx))),
                    ),
            )),
            AuthMethod::Agent => stack().child(hint("使用 SSH_AUTH_SOCK 指向的 ssh-agent 里的密钥（如 1Password、macOS 钥匙串代理）。", p)),
            AuthMethod::KeyboardInteractive => stack().child(hint(
                "服务端会依次发起提示（如密码 → 动态验证码），连接时弹出对应输入框；密码可存钥匙串，验证码每次手动输入。",
                p,
            )),
        }
    }

    fn render_jumps(&self, p: &Palette, cx: &mut Context<Self>) -> gpui::Div {
        let rows = self.jumps.iter().enumerate().map(|(ix, id)| {
            let small = |id: (&'static str, usize), icon: Lucide| {
                Button::new(id).ghost().compact().icon(Icon::new(icon).size(px(13.)))
            };
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(32.))
                .px(px(10.))
                .rounded(px(9.))
                .bg(p.card2)
                .child(div().w(px(18.)).text_size(px(11.5)).text_color(p.tx3).child((ix + 1).to_string()))
                .child(div().flex_1().min_w_0().truncate().text_size(px(13.)).text_color(p.tx).child(self.name_of(id)))
                .child(
                    small(("jump-up", ix), Lucide::ArrowUp)
                        .on_click(cx.listener(move |this, _, _, cx| this.move_jump(ix, -1, cx))),
                )
                .child(
                    small(("jump-down", ix), Lucide::ArrowDown)
                        .on_click(cx.listener(move |this, _, _, cx| this.move_jump(ix, 1, cx))),
                )
                .child(small(("jump-remove", ix), Lucide::X).on_click(cx.listener(move |this, _, _, cx| {
                    this.jumps.remove(ix);
                    this.error = None;
                    cx.notify();
                })))
        });
        let free: Vec<_> = self.choices.iter().filter(|(id, _)| !self.jumps.contains(id)).collect();
        let route: Vec<String> = std::iter::once("本机".to_string())
            .chain(self.jumps.iter().map(|id| self.name_of(id)))
            .chain(
                std::iter::once(self.name.read(cx).value().to_string())
                    .map(|n| if n.trim().is_empty() { "此主机".into() } else { n }),
            )
            .collect();
        stack()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.tx2)
                            .child("跳板机 ProxyJump"),
                    )
                    .child(div().text_size(px(11.5)).text_color(p.tx3).child("按顺序逐跳连接，每跳独立校验指纹")),
            )
            .children(rows)
            .children((!free.is_empty()).then(|| {
                div().flex().flex_wrap().gap(px(6.)).children(free.into_iter().map(|(id, name)| {
                    let id = id.clone();
                    self.chip(SharedString::from(format!("add-jump-{id}")), format!("＋ {name}"), false, p).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.jumps.push(id.clone());
                            this.error = None;
                            cx.notify();
                        }),
                    )
                }))
            }))
            .children(
                (self.jumps.is_empty() && self.choices.is_empty()).then(|| hint("还没有其他主机可以用作跳板机。", p)),
            )
            .children((!self.jumps.is_empty()).then(|| {
                div().text_size(px(12.)).font_family(crate::theme::MONO_FONT).text_color(p.tx3).child(route.join(" → "))
            }))
    }
}

impl DialogBody for HostEditor {
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let home = std::env::home_dir();
        let draft = match build_draft(&self.fields(cx), self.original.as_ref(), home.as_deref()) {
            Ok(draft) => draft,
            Err(message) => {
                self.error = Some(message.into());
                cx.notify();
                return false;
            }
        };
        let saved = self.workspace.update(cx, |workspace, cx| workspace.save_host_draft(draft, window, cx));
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

    fn cancel(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.name.update(cx, |input, cx| input.focus(window, cx));
    }
}

impl Render for HostEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let this = cx.entity().downgrade();

        let protocols = [Protocol::Ssh, Protocol::Mosh, Protocol::Telnet, Protocol::Serial]
            .iter()
            .map(|protocol| {
                let item = SegmentItem::new(protocol.label(), *protocol == self.protocol);
                if *protocol == Protocol::Ssh { item } else { item.disabled("P7 实现") }
            })
            .collect();
        let auths = AUTH_ORDER.iter().map(|auth| SegmentItem::new(auth.label(), *auth == self.auth)).collect();
        let auth_picker = segmented(
            "editor-auth",
            auths,
            move |index, _, cx| {
                this.update(cx, |editor, cx| {
                    editor.auth = AUTH_ORDER[index];
                    editor.error = None;
                    cx.notify();
                })
                .ok();
            },
            cx,
        );

        let current_group = self.group.read(cx).value().to_string();
        let group_chips = (!self.groups.is_empty()).then(|| {
            div().flex().flex_wrap().gap(px(6.)).children(self.groups.iter().enumerate().map(|(ix, name)| {
                let picked = name.clone();
                self.chip(("group-chip", ix), name.clone(), *name == current_group, &p).on_click(
                    cx.listener(move |this, _, window, cx| this.set_text(&this.group.clone(), &picked, window, cx)),
                )
            }))
        });

        let import_link = self.original.is_none().then(|| {
            div().flex().justify_end().child(
                button("editor-import", "从 ~/.ssh/config 导入", Tone::Plain, true).on_click(cx.listener(
                    |this, _, window, cx| {
                        window.close_dialog(cx);
                        this.workspace.update(cx, |workspace, cx| workspace.open_import(window, cx)).ok();
                    },
                )),
            )
        });

        stack()
            .children(import_link)
            .child(div().flex().child(segmented("editor-protocol", protocols, |_, _, _| {}, cx).flex_none()))
            .child(field("名称", &p, Input::new(&self.name)))
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().flex_1().child(field("主机地址 HostName", &p, Input::new(&self.addr))))
                    .child(div().w(px(110.)).child(field("端口", &p, Input::new(&self.port)))),
            )
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().flex_1().child(field("用户 User", &p, Input::new(&self.user))))
                    .child(div().flex_1().child(field("标签（逗号分隔）", &p, Input::new(&self.tags)))),
            )
            .child(field(
                "分组（输入新名称会新建分组）",
                &p,
                stack().gap(px(8.)).child(Input::new(&self.group)).children(group_chips),
            ))
            .child(field("认证方式", &p, auth_picker))
            .child(self.render_auth_details(&p, cx))
            .child(hint("密码存入 macOS 钥匙串；本地配置库中不保存任何明文凭据。", &p))
            .child(self.render_jumps(&p, cx))
            .children(self.error.clone().map(|message| notice(Notice::Error, message, &p)))
            .child(actions(vec![
                button("editor-cancel", "取消", Tone::Plain, true)
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                button("editor-save", "保存", Tone::Primary, true)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }
}
