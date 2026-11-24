//! Builtin registry and input decisions, independent of GPUI and durable history.
use shellmate_ai::PermissionMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Local,
    Agent,
    #[allow(dead_code)]
    Prompt(&'static str),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhileBusy {
    Run,
    Queue,
    Refuse,
}
#[derive(Clone, Copy, Debug)]
pub struct Command {
    pub name: &'static str,
    pub args_hint: &'static str,
    pub description: &'static str,
    pub kind: Kind,
    pub while_busy: WhileBusy,
}
pub const COMMANDS: &[Command] = &[
    Command {
        name: "compact",
        args_hint: "[重点]",
        description: "压缩上下文，保留继续任务所需的信息",
        kind: Kind::Agent,
        while_busy: WhileBusy::Queue,
    },
    Command {
        name: "clear",
        args_hint: "",
        description: "开始新对话，旧对话保留在历史中",
        kind: Kind::Local,
        while_busy: WhileBusy::Refuse,
    },
    Command {
        name: "context",
        args_hint: "",
        description: "查看上下文用量与自动压缩阈值",
        kind: Kind::Local,
        while_busy: WhileBusy::Run,
    },
    Command {
        name: "tasks",
        args_hint: "",
        description: "查看后台任务，停止运行中的命令",
        kind: Kind::Local,
        while_busy: WhileBusy::Run,
    },
    Command {
        name: "mode",
        args_hint: "[ask|auto|full]",
        description: "查看或切换此主机的审批模式",
        kind: Kind::Local,
        while_busy: WhileBusy::Refuse,
    },
    Command {
        name: "help",
        args_hint: "",
        description: "查看全部命令与用法",
        kind: Kind::Local,
        while_busy: WhileBusy::Run,
    },
];
pub fn mode(args: &str) -> Option<PermissionMode> {
    match args {
        "ask" => Some(PermissionMode::Ask),
        "auto" => Some(PermissionMode::Auto),
        "full" => Some(PermissionMode::Full),
        _ => None,
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Dispatch {
    Question(String),
    OpenContext,
    Run { name: &'static str, args: String },
    QueueCompact(Option<String>),
    Hint(String),
}
pub const STOP_FIRST: &str = "请先停止当前任务";
pub const QUEUED: &str = "已排队：将在当前步骤结束后压缩";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposerKey {
    Native,
    Previous,
    Next,
    Complete,
    Submit,
    Dismiss,
}
/// Native IME and Shift+Enter always retain their editing semantics. Only the
/// open command menu intercepts navigation; closed-menu Enter uses PressEnter.
pub fn composer_key(popup: bool, key: &str, shift: bool, composing: bool) -> ComposerKey {
    if composing || !popup {
        return ComposerKey::Native;
    }
    match key {
        "up" => ComposerKey::Previous,
        "down" => ComposerKey::Next,
        "tab" => ComposerKey::Complete,
        "enter" if !shift => ComposerKey::Submit,
        "escape" => ComposerKey::Dismiss,
        _ => ComposerKey::Native,
    }
}
pub fn submits_enter(shift: bool, composing: bool) -> bool {
    !shift && !composing
}
pub fn dispatch(text: &str, busy: bool, registry: &[Command]) -> Dispatch {
    let (token, args) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(a, b)| (a, b.trim()));
    let command = token
        .strip_prefix('/')
        .and_then(|name| registry.iter().find(|c| c.name == name));
    let Some(c) = command else {
        return if busy {
            Dispatch::Hint(STOP_FIRST.into())
        } else {
            Dispatch::Question(text.into())
        };
    };
    if busy && c.while_busy == WhileBusy::Refuse {
        return Dispatch::Hint(STOP_FIRST.into());
    }
    if (c.name == "mode" && !args.is_empty() && mode(args).is_none())
        || (c.args_hint.is_empty() && !args.is_empty())
    {
        return Dispatch::Hint(
            format!("用法：/{} {}", c.name, c.args_hint)
                .trim_end()
                .into(),
        );
    }
    if c.name == "context" {
        return Dispatch::OpenContext;
    }
    match c.kind {
        Kind::Prompt(template) => {
            if busy {
                Dispatch::Hint(STOP_FIRST.into())
            } else {
                Dispatch::Question(template.replace("{args}", args))
            }
        }
        _ if busy && c.while_busy == WhileBusy::Queue => {
            Dispatch::QueueCompact((!args.is_empty()).then(|| args.into()))
        }
        _ => Dispatch::Run {
            name: c.name,
            args: args.into(),
        },
    }
}
#[derive(Default)]
pub struct Popup {
    pub open: bool,
    pub selected: usize,
    pub navigated: bool,
    text: String,
}
impl Popup {
    pub fn changed(&mut self, text: &str) {
        if self.text != text {
            self.text = text.into();
            self.selected = 0;
            self.navigated = false;
            self.open = text.starts_with('/') && !self.matches().is_empty();
        }
    }
    pub fn matches(&self) -> Vec<&'static Command> {
        let prefix = self
            .text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .strip_prefix('/');
        COMMANDS
            .iter()
            .filter(|c| prefix.is_some_and(|p| c.name.starts_with(p)))
            .collect()
    }
    pub fn navigate(&mut self, down: bool) {
        self.navigated = true;
        let len = self.matches().len();
        if len > 0 {
            self.selected = (self.selected + if down { 1 } else { len - 1 }) % len;
        }
    }
    pub fn complete(&self) -> Option<String> {
        self.matches().get(self.selected).map(|c| {
            format!(
                "/{}{}",
                c.name,
                if c.args_hint.is_empty() { "" } else { " " }
            )
        })
    }
    pub fn escape(&mut self) {
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn textarea_keys_preserve_newlines_and_ime_but_menu_owns_navigation() {
        for key in ["up", "down", "tab", "enter", "escape"] {
            for shift in [false, true] {
                assert_eq!(composer_key(true, key, shift, true), ComposerKey::Native);
                assert_eq!(composer_key(false, key, shift, false), ComposerKey::Native);
            }
        }
        for (key, action) in [
            ("up", ComposerKey::Previous),
            ("down", ComposerKey::Next),
            ("tab", ComposerKey::Complete),
            ("enter", ComposerKey::Submit),
            ("escape", ComposerKey::Dismiss),
        ] {
            assert_eq!(composer_key(true, key, false, false), action);
        }
        assert_eq!(
            composer_key(true, "enter", true, false),
            ComposerKey::Native
        );
        assert!(submits_enter(false, false));
        for (shift, composing) in [(true, false), (false, true), (true, true)] {
            assert!(!submits_enter(shift, composing));
        }
    }
    #[test]
    fn exact_only_and_args() {
        for text in [
            "/etc/nginx/nginx.conf 有什么问题",
            "/compactx",
            "/co",
            " /help",
            "普通问题",
        ] {
            assert_eq!(
                dispatch(text, false, COMMANDS),
                Dispatch::Question(text.into())
            );
        }
        assert_eq!(
            dispatch("/compact  保留部署\n步骤  ", false, COMMANDS),
            Dispatch::Run {
                name: "compact",
                args: "保留部署\n步骤".into()
            }
        );
        for text in ["/mode foo", "/mode auto full"] {
            assert_eq!(
                dispatch(text, false, COMMANDS),
                Dispatch::Hint("用法：/mode [ask|auto|full]".into())
            );
        }
        assert!(matches!(
            dispatch("/help extra", false, COMMANDS),
            Dispatch::Hint(_)
        ));
        assert_eq!(mode("full"), Some(PermissionMode::Full));
    }
    #[test]
    fn prompt_expands_to_a_question() {
        let registry = [Command {
            name: "review",
            args_hint: "[文件]",
            description: "fixture",
            kind: Kind::Prompt("请检查 {args} 的错误"),
            while_busy: WhileBusy::Refuse,
        }];
        assert_eq!(
            dispatch("/review app.rs", false, &registry),
            Dispatch::Question("请检查 app.rs 的错误".into())
        );
    }
    #[test]
    fn busy_policy() {
        for busy in [false, true] {
            assert_eq!(dispatch("/context", busy, COMMANDS), Dispatch::OpenContext);
        }
        assert!(matches!(
            dispatch("/context extra", false, COMMANDS),
            Dispatch::Hint(_)
        ));
        assert_eq!(
            dispatch("/compact 任务重点", true, COMMANDS),
            Dispatch::QueueCompact(Some("任务重点".into()))
        );
        for text in ["/clear", "/mode", "/mode auto", "普通问题", "/compactx"] {
            assert_eq!(
                dispatch(text, true, COMMANDS),
                Dispatch::Hint(STOP_FIRST.into())
            );
        }
        for text in ["/tasks", "/help"] {
            assert!(matches!(
                dispatch(text, true, COMMANDS),
                Dispatch::Run { .. }
            ));
        }
    }
    #[test]
    fn popup_filter_wrap_completion_escape() {
        let mut p = Popup::default();
        p.changed("/");
        assert_eq!(p.matches().len(), 6);
        assert_eq!(p.selected, 0);
        p.navigate(false);
        assert_eq!(p.complete().as_deref(), Some("/help"));
        p.navigate(true);
        assert_eq!(p.complete().as_deref(), Some("/compact "));
        p.changed("/co");
        assert_eq!(
            p.matches().iter().map(|c| c.name).collect::<Vec<_>>(),
            ["compact", "context"]
        );
        p.navigate(true);
        assert_eq!(p.complete().as_deref(), Some("/context"));
        p.escape();
        assert!(!p.open);
        assert_eq!(p.text, "/co");
        p.changed("/co");
        assert!(!p.open);
        p.changed("/etc/nginx");
        assert!(!p.open);
    }
}
