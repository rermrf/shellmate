//! What the 端口转发 panel says: the new-rule form and its checks, the cards of the rules and how
//! each state is worded, the summary on top and the confirmations. No gpui, so it is tested as text.

use shellmate_forward::{
    AddrError, Endpoint, ForwardKind, ForwardRule, RuleError, RuleId, RuleState, StatsSnapshot, parse_listen,
    parse_target,
};
use shellmate_sftp::format_size;

/// Shown for a local or dynamic rule that listens beyond this machine.
pub const LAN_WARNING: &str = "局域网内其他设备也能访问此端口";
/// Shown as well for a dynamic rule (SOCKS5 proxy) that listens beyond this machine.
pub const OPEN_PROXY_WARNING: &str = "这相当于开放代理";
/// Shown for a remote rule that asks the server to listen on all addresses.
pub const GATEWAY_HINT: &str = "服务器未开启 GatewayPorts 时只能在远端本机访问";
/// Shown for the rules of a connection whose host is not saved.
pub const TEMPORARY_NOTE: &str = "临时连接的规则不会保存";
/// The line under the title of the top card.
pub const AUTO_RESTORE_NOTE: &str = "断线重连后自动恢复";

/// The warnings a rule that listens on `listen` deserves (the same lines in the form and on the card).
pub fn warnings(kind: ForwardKind, listen: &Endpoint) -> Vec<&'static str> {
    match kind {
        ForwardKind::Local | ForwardKind::Dynamic if !listen.is_loopback() => {
            let mut lines = vec![LAN_WARNING];
            if kind == ForwardKind::Dynamic {
                lines.push(OPEN_PROXY_WARNING);
            }
            lines
        }
        ForwardKind::Remote if matches!(listen.host.as_str(), "0.0.0.0" | "::" | "*") => vec![GATEWAY_HINT],
        _ => Vec::new(),
    }
}

/// The name of a kind as the cards and the form's segments write it.
pub fn kind_name(kind: ForwardKind) -> &'static str {
    match kind {
        ForwardKind::Local => "本地转发",
        ForwardKind::Remote => "远程转发",
        ForwardKind::Dynamic => "动态 SOCKS5",
    }
}

/// The labels of the form's three segments.
pub fn segment_label(kind: ForwardKind) -> &'static str {
    match kind {
        ForwardKind::Local => "-L 本地",
        ForwardKind::Remote => "-R 远程",
        ForwardKind::Dynamic => "-D 动态",
    }
}

/// The kinds in the order of the segments.
pub const KINDS: [ForwardKind; 3] = [ForwardKind::Local, ForwardKind::Remote, ForwardKind::Dynamic];

// ---- the form ----------------------------------------------------------------------------------

/// What is typed in the new-rule form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleForm {
    /// The segment picked.
    pub kind: ForwardKind,
    /// 规则名称 (empty: named after the rule, like `-L 3306`).
    pub name: String,
    /// The listen address.
    pub listen: String,
    /// The target address (not used by a dynamic rule).
    pub target: String,
}

impl Default for RuleForm {
    fn default() -> Self {
        Self { kind: ForwardKind::Local, name: String::new(), listen: String::new(), target: String::new() }
    }
}

/// What the checks of the form found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormCheck {
    /// The problem with the listen address, if any.
    pub listen_error: Option<String>,
    /// The problem with the target address, if any.
    pub target_error: Option<String>,
    /// The warnings of the (valid) listen address.
    pub warnings: Vec<&'static str>,
    /// The rule the form makes, when there is nothing to complain about.
    pub rule: Option<ForwardRule>,
}

impl RuleForm {
    /// Placeholder of the listen field.
    pub fn listen_placeholder(&self) -> &'static str {
        match self.kind {
            ForwardKind::Remote => "远端监听 0.0.0.0:9000",
            ForwardKind::Local | ForwardKind::Dynamic => "本地监听 127.0.0.1:5432",
        }
    }

    /// Placeholder of the target field.
    pub fn target_placeholder(&self) -> &'static str {
        match self.kind {
            ForwardKind::Remote => "转发到本机 localhost:3000",
            ForwardKind::Local | ForwardKind::Dynamic => "目标 db.internal:5432",
        }
    }

    /// What replaces the target field of a dynamic rule.
    pub const DYNAMIC_TARGET: &'static str = "SOCKS5 代理，目标由客户端决定";

    /// Checks the form against the rules the host already has. `id` is the id of the rule the
    /// form will make.
    pub fn check(&self, existing: &[ForwardRule], id: RuleId) -> FormCheck {
        let listen = parse_listen(&self.listen, self.kind);
        let target = match self.kind {
            ForwardKind::Dynamic => Ok(None),
            ForwardKind::Local | ForwardKind::Remote => parse_target(&self.target).map(Some),
        };
        let mut listen_error = listen.as_ref().err().map(|err| match err {
            AddrError::Empty => "请填写监听地址（只写端口也行，如 5432）".to_string(),
            other => other.to_string(),
        });
        let target_error = target.as_ref().err().map(|err| match err {
            AddrError::Empty => "请填写目标地址（如 db.internal:5432）".to_string(),
            other => other.to_string(),
        });
        let mut warnings = Vec::new();
        if let Ok(listen) = &listen {
            warnings = self::warnings(self.kind, listen);
            if let Some(clash) = clash(self.kind, listen, existing) {
                listen_error = Some(clash);
            }
        }
        let rule = match (listen, target, &listen_error, &target_error) {
            (Ok(listen), Ok(target), None, None) => {
                let mut rule = ForwardRule {
                    id,
                    name: self.name.trim().to_string(),
                    kind: self.kind,
                    listen,
                    target,
                    enabled: true,
                };
                if rule.name.is_empty() {
                    rule.name = rule.default_name();
                }
                Some(rule)
            }
            _ => None,
        };
        FormCheck { listen_error, target_error, warnings, rule }
    }
}

/// An existing rule that would listen on the same port: on this machine for local and dynamic rules,
/// on the server for remote ones (port 0 lets the server choose, so it never clashes).
fn clash(kind: ForwardKind, listen: &Endpoint, existing: &[ForwardRule]) -> Option<String> {
    if listen.port == 0 {
        return None;
    }
    let remote = kind == ForwardKind::Remote;
    existing.iter().find(|other| (other.kind == ForwardKind::Remote) == remote && other.listen == *listen).map(
        |other| {
            let side = if remote { "远端" } else { "本机" };
            format!("已有规则「{}」监听{side} {listen}", other.name)
        },
    )
}

// ---- the cards ---------------------------------------------------------------------------------

/// The color of a card's status dot and text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Running (green).
    Ok,
    /// Waiting for something (yellow).
    Waiting,
    /// Switched off (grey).
    Off,
    /// Failed (red).
    Failed,
}

/// What a retry button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryAction {
    /// The local port is taken: listen on this one from now on.
    OtherPort(u16),
    /// Ask the server for the remote port again.
    Again,
}

/// The button of a rule that could not start, or that waits for the server to let go of its port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryButton {
    /// What it does.
    pub action: RetryAction,
    /// `换用 18080 重试`, `重试` or `立即重试`.
    pub label: String,
}

/// What a card says while the server still holds the port of a remote rule that worked before.
pub fn port_held_text(port: u16) -> String {
    format!("远端端口 {port} 仍被占用（可能是断线前的会话还没被服务器回收）· 正在重试")
}

/// What the panel knows about a rule that is not running (yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleRuntime {
    /// Switched off.
    Disabled,
    /// Enabled, but the connection has not come up to start it.
    NotStarted,
    /// Started; where it stands and what has gone through it.
    Started {
        /// The state.
        state: RuleState,
        /// Connections and traffic.
        stats: StatsSnapshot,
    },
}

/// One rule as the panel draws it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleCard {
    /// The rule.
    pub id: RuleId,
    /// Its kind (the color and the letter of the square).
    pub kind: ForwardKind,
    /// `-L`, `-R` or `-D`.
    pub flag: &'static str,
    /// The rule's name.
    pub name: String,
    /// `本地转发`, `远程转发` or `动态 SOCKS5`.
    pub type_name: &'static str,
    /// `监听 → 目标`, in monospace.
    pub route: String,
    /// `运行中 · 2 个连接`, `等待重连`, `已停用`, or why the rule failed.
    pub status: String,
    /// The color of the status.
    pub tone: Tone,
    /// `↓ 18.4 MB · ↑ 1.2 MB` while there is something to say.
    pub traffic: Option<String>,
    /// The button of a rule that could not start or waits for its port.
    pub retry: Option<RetryButton>,
    /// Where the toggle stands.
    pub enabled: bool,
    /// Warnings about what the rule opens up.
    pub warnings: Vec<&'static str>,
    /// Open connections, for the confirmation of a deletion.
    pub connections: u32,
}

/// `↓ 18.4 MB · ↑ 1.2 MB`: what came from the SSH side and what was sent to it.
pub fn traffic(stats: &StatsSnapshot) -> String {
    format!("↓ {} · ↑ {}", format_size(stats.down), format_size(stats.up))
}

/// `监听 → 目标` of a rule (a remote rule says which side each address is on; `host` names the
/// connection a dynamic rule goes out through). A remote rule that asked for port 0 shows the
/// port the server gave it.
pub fn route_text(rule: &ForwardRule, host: &str, actual_port: Option<u16>) -> String {
    let listen = Endpoint { host: rule.listen.host.clone(), port: actual_port.unwrap_or(rule.listen.port) };
    match (&rule.kind, &rule.target) {
        (ForwardKind::Remote, Some(target)) => format!("远端 {listen} → 本机 {target}"),
        (_, Some(target)) => format!("{listen} → {target}"),
        (_, None) => format!("{listen} → 经 {host} 出口"),
    }
}

/// The card of `rule` for the runtime it has; `host` is the connection's host name.
pub fn card(rule: &ForwardRule, runtime: &RuleRuntime, host: &str) -> RuleCard {
    let mut card = RuleCard {
        id: rule.id.clone(),
        kind: rule.kind,
        flag: rule.kind.flag(),
        name: rule.name.clone(),
        type_name: kind_name(rule.kind),
        route: route_text(rule, host, None),
        status: String::new(),
        tone: Tone::Off,
        traffic: None,
        retry: None,
        enabled: rule.enabled,
        warnings: warnings(rule.kind, &rule.listen),
        connections: 0,
    };
    match runtime {
        RuleRuntime::Disabled => card.status = "已停用".into(),
        RuleRuntime::NotStarted => {
            card.status = "等待连接".into();
            card.tone = Tone::Waiting;
        }
        RuleRuntime::Started { state, stats } => {
            card.connections = stats.active;
            let has_traffic = stats.up > 0 || stats.down > 0;
            match state {
                RuleState::Running { port } => {
                    card.status = format!("运行中 · {} 个连接", stats.active);
                    card.tone = Tone::Ok;
                    card.traffic = Some(traffic(stats));
                    card.route = route_text(rule, host, Some(*port));
                }
                RuleState::Starting => {
                    card.status = "正在启动…".into();
                    card.tone = Tone::Waiting;
                }
                RuleState::WaitingReconnect => {
                    card.status = "等待重连".into();
                    card.tone = Tone::Waiting;
                    card.traffic = has_traffic.then(|| traffic(stats));
                }
                RuleState::RemotePortHeld { port } => {
                    card.status = port_held_text(*port);
                    card.tone = Tone::Waiting;
                    card.traffic = has_traffic.then(|| traffic(stats));
                    card.retry = Some(RetryButton { action: RetryAction::Again, label: "立即重试".into() });
                }
                RuleState::Failed(error) => {
                    card.status = error.to_string();
                    card.tone = Tone::Failed;
                    card.retry = retry_button(error);
                }
                RuleState::Stopped => {
                    card.status = "已停用".into();
                }
            }
        }
    }
    card
}

fn retry_button(error: &RuleError) -> Option<RetryButton> {
    match error {
        RuleError::RemoteDenied | RuleError::Remote(_) => {
            Some(RetryButton { action: RetryAction::Again, label: "重试".into() })
        }
        other => other
            .suggested_port()
            .map(|port| RetryButton { action: RetryAction::OtherPort(port), label: format!("换用 {port} 重试") }),
    }
}

/// The rule with its listen port replaced, for 换用 M 重试.
pub fn with_listen_port(rule: &ForwardRule, port: u16) -> ForwardRule {
    let mut moved = rule.clone();
    moved.listen.port = port;
    moved
}

// ---- the panel as a whole ----------------------------------------------------------------------

/// The card on top: how many rules run and through what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// `2 条规则运行中`.
    pub title: String,
    /// `经由 prod-web-01 的 SSH 连接 · 断线重连后自动恢复`.
    pub subtitle: String,
    /// What else the user should know: the rules are not saved, or another connection carries them.
    pub notes: Vec<String>,
}

/// Everything a panel shows, as the workspace hands it over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardView {
    /// The top card.
    pub summary: Summary,
    /// The rules in their order.
    pub cards: Vec<RuleCard>,
    /// The rules themselves, for the form's check for clashes.
    pub rules: Vec<ForwardRule>,
}

impl ForwardView {
    /// A panel with nothing in it (before the workspace has said anything).
    pub fn empty(host: &str) -> Self {
        summary_view(host, &[], &[], 0, &[])
    }
}

/// The view for `rules` of the connection to `host`: `runtime` says what each rule does now (by
/// position), `running` is how many run and `notes` what the panel adds besides.
pub fn summary_view(
    host: &str,
    rules: &[ForwardRule],
    runtime: &[RuleRuntime],
    running: usize,
    notes: &[String],
) -> ForwardView {
    let cards = rules
        .iter()
        .enumerate()
        .map(|(ix, rule)| card(rule, runtime.get(ix).unwrap_or(&RuleRuntime::NotStarted), host))
        .collect();
    ForwardView {
        summary: Summary {
            title: format!("{running} 条规则运行中"),
            subtitle: format!("经由 {host} 的 SSH 连接 · {AUTO_RESTORE_NOTE}"),
            notes: notes.to_vec(),
        },
        cards,
        rules: rules.to_vec(),
    }
}

/// The empty state: what the three kinds are for.
pub const EMPTY_TITLE: &str = "还没有端口转发规则";
/// The three lines of the empty state.
pub const EMPTY_KINDS: [(&str, &str); 3] = [
    ("-L 本地转发", "本机端口 → 服务器侧地址"),
    ("-R 远程转发", "服务器端口 → 本机服务"),
    ("-D 动态转发", "本机 SOCKS5 代理"),
];

// ---- confirmations -----------------------------------------------------------------------------

/// The question before the last tab of a connection with running rules is closed: the title and the text.
pub fn close_confirmation(host: &str, running: usize, connections: u32) -> (String, String) {
    let mut text = format!("关闭后 {running} 条端口转发会停止");
    if connections > 0 {
        text.push_str(&format!("（当前 {connections} 个连接会断开）"));
    }
    (format!("关闭 {host} 的最后一个标签？"), text)
}

/// The question before a rule is deleted: the title and the text (what the rule is, and that open
/// connections are cut).
pub fn delete_confirmation(card: &RuleCard) -> (String, String) {
    let mut text = String::new();
    if card.connections > 0 {
        text.push_str(&format!("当前 {} 个活动连接会被断开。\n", card.connections));
    }
    text.push_str(&format!("{}  {}", card.flag, card.route));
    (format!("删除规则「{}」？", card.name), text)
}

#[cfg(test)]
mod tests {
    use shellmate_forward::RuleId;

    use super::*;

    fn rule(kind: ForwardKind, listen: &str, target: &str) -> ForwardRule {
        ForwardRule::parse(RuleId::generate(), "", kind, listen, target, true).unwrap()
    }

    fn form(kind: ForwardKind, listen: &str, target: &str) -> RuleForm {
        RuleForm { kind, name: String::new(), listen: listen.into(), target: target.into() }
    }

    fn check(form: &RuleForm, existing: &[ForwardRule]) -> FormCheck {
        form.check(existing, RuleId::from_raw("new"))
    }

    // ---- the form ----

    #[test]
    fn a_valid_form_makes_a_named_enabled_rule() {
        let result = check(&form(ForwardKind::Local, "5433", "db.internal:5432"), &[]);
        let rule = result.rule.expect("valid");
        assert_eq!((rule.name.as_str(), rule.enabled), ("-L 5433", true), "named after the rule when no name is typed");
        assert_eq!(rule.route_text(), "127.0.0.1:5433 → db.internal:5432");
        assert_eq!((result.listen_error, result.target_error, result.warnings), (None, None, vec![]));

        let named = RuleForm { name: "  MySQL 主库 ".into(), ..form(ForwardKind::Local, "3306", "10.0.2.8:3306") };
        assert_eq!(check(&named, &[]).rule.unwrap().name, "MySQL 主库");
        assert_eq!(check(&named, &[]).rule.unwrap().id, RuleId::from_raw("new"));
    }

    #[test]
    fn a_dynamic_rule_needs_no_target() {
        let result = check(&form(ForwardKind::Dynamic, "1080", "garbage that is ignored"), &[]);
        assert!(result.target_error.is_none());
        let rule = result.rule.unwrap();
        assert_eq!((rule.target, rule.name.as_str()), (None, "-D 1080"));
    }

    #[test]
    fn errors_are_the_forward_parsers_chinese_messages_next_to_the_field() {
        let result = check(&form(ForwardKind::Local, "0", "db.internal"), &[]);
        assert_eq!(
            result.listen_error.as_deref(),
            Some("本地监听端口不能为 0，需要在 1–65535 之间（只有远程转发可以填 0，表示由服务器分配）")
        );
        assert_eq!(result.target_error.as_deref(), Some("缺少端口，请写成「主机:端口」"));
        assert!(result.rule.is_none());

        let result = check(&form(ForwardKind::Local, "::1:80", "[::1]:99999"), &[]);
        assert_eq!(result.listen_error.as_deref(), Some("IPv6 地址需要用方括号括起来，例如 [::1]:3306"));
        assert_eq!(result.target_error.as_deref(), Some("端口 99999 超出范围，需要在 1–65535 之间"));

        let result = check(&form(ForwardKind::Local, "db.internal:5432", "x:1"), &[]);
        assert!(result.listen_error.unwrap().contains("不能写主机名"), "a local listen address is an IP");
    }

    #[test]
    fn empty_fields_ask_for_what_is_missing() {
        let result = check(&form(ForwardKind::Local, "", "  "), &[]);
        assert_eq!(result.listen_error.as_deref(), Some("请填写监听地址（只写端口也行，如 5432）"));
        assert_eq!(result.target_error.as_deref(), Some("请填写目标地址（如 db.internal:5432）"));
        let result = check(&form(ForwardKind::Dynamic, "", ""), &[]);
        assert!(result.listen_error.is_some() && result.target_error.is_none());
    }

    #[test]
    fn a_remote_rule_may_use_port_zero_and_a_local_one_may_not() {
        assert!(check(&form(ForwardKind::Remote, "0", "localhost:3000"), &[]).rule.is_some());
        assert!(check(&form(ForwardKind::Remote, "localhost:0", "localhost:3000"), &[]).rule.is_some());
        assert!(check(&form(ForwardKind::Local, "0", "h:1"), &[]).listen_error.is_some());
    }

    #[test]
    fn listening_where_another_rule_listens_is_refused_inline() {
        let mut existing = rule(ForwardKind::Local, "3306", "db:3306");
        existing.name = "MySQL 主库".into();
        let remote = rule(ForwardKind::Remote, "9000", "localhost:3000");
        let rules = [existing, remote];
        let clash = check(&form(ForwardKind::Local, "127.0.0.1:3306", "other:3306"), &rules);
        assert_eq!(clash.listen_error.as_deref(), Some("已有规则「MySQL 主库」监听本机 127.0.0.1:3306"));
        assert!(clash.rule.is_none());
        // A dynamic rule listens on this machine too.
        assert!(check(&form(ForwardKind::Dynamic, "3306", ""), &rules).listen_error.is_some());
        // The server's ports and this machine's are different things.
        assert!(check(&form(ForwardKind::Local, "9000", "x:1"), &rules).rule.is_some());
        assert!(check(&form(ForwardKind::Remote, "3306", "x:1"), &rules).rule.is_some());
        let remote_clash = check(&form(ForwardKind::Remote, "127.0.0.1:9000", "x:1"), &rules);
        assert_eq!(remote_clash.listen_error.as_deref(), Some("已有规则「-R 9000」监听远端 127.0.0.1:9000"));
        // Another address on the same port is another listener.
        assert!(check(&form(ForwardKind::Local, "[::1]:3306", "x:1"), &rules).rule.is_some());
    }

    #[test]
    fn placeholders_follow_the_kind() {
        assert_eq!(form(ForwardKind::Local, "", "").listen_placeholder(), "本地监听 127.0.0.1:5432");
        assert_eq!(form(ForwardKind::Local, "", "").target_placeholder(), "目标 db.internal:5432");
        assert_eq!(form(ForwardKind::Remote, "", "").listen_placeholder(), "远端监听 0.0.0.0:9000");
        assert_eq!(form(ForwardKind::Remote, "", "").target_placeholder(), "转发到本机 localhost:3000");
        assert_eq!(form(ForwardKind::Dynamic, "", "").listen_placeholder(), "本地监听 127.0.0.1:5432");
        assert_eq!(RuleForm::DYNAMIC_TARGET, "SOCKS5 代理，目标由客户端决定");
        assert_eq!(KINDS.map(segment_label), ["-L 本地", "-R 远程", "-D 动态"]);
    }

    // ---- warnings ----

    #[test]
    fn listening_beyond_this_machine_is_warned_about() {
        let lan = |kind, listen: &str| warnings(kind, &parse_listen(listen, kind).unwrap());
        assert!(lan(ForwardKind::Local, "3306").is_empty());
        assert!(lan(ForwardKind::Local, "[::1]:3306").is_empty());
        assert_eq!(lan(ForwardKind::Local, "0.0.0.0:3306"), ["局域网内其他设备也能访问此端口"]);
        assert_eq!(lan(ForwardKind::Local, "192.168.1.5:3306"), ["局域网内其他设备也能访问此端口"]);
        assert_eq!(lan(ForwardKind::Local, "[::]:3306"), ["局域网内其他设备也能访问此端口"]);
        assert_eq!(lan(ForwardKind::Dynamic, "1080"), Vec::<&str>::new());
        assert_eq!(
            lan(ForwardKind::Dynamic, "0.0.0.0:1080"),
            ["局域网内其他设备也能访问此端口", "这相当于开放代理"],
            "a SOCKS proxy for the LAN is an open proxy"
        );
    }

    #[test]
    fn a_remote_rule_on_all_addresses_gets_the_gateway_ports_hint() {
        let hint = |listen: &str| warnings(ForwardKind::Remote, &parse_listen(listen, ForwardKind::Remote).unwrap());
        assert_eq!(hint("0.0.0.0:9000"), ["服务器未开启 GatewayPorts 时只能在远端本机访问"]);
        assert_eq!(hint("[::]:9000"), ["服务器未开启 GatewayPorts 时只能在远端本机访问"]);
        assert!(hint("9000").is_empty() && hint("localhost:9000").is_empty());
        // The form shows them as the address is typed.
        let result = check(&form(ForwardKind::Remote, "0.0.0.0:9000", "localhost:3000"), &[]);
        assert_eq!(result.warnings, [GATEWAY_HINT]);
        assert!(result.rule.is_some(), "a warning does not stop the rule");
    }

    // ---- the cards ----

    fn stats(active: u32, down: u64, up: u64) -> StatsSnapshot {
        StatsSnapshot { active, down, up }
    }

    fn started(state: RuleState, stats: StatsSnapshot) -> RuleRuntime {
        RuleRuntime::Started { state, stats }
    }

    #[test]
    fn a_running_rule_shows_connections_and_traffic_like_the_prototype() {
        let mut rule = rule(ForwardKind::Local, "3306", "10.0.2.8:3306");
        rule.name = "MySQL 主库".into();
        let card =
            card(&rule, &started(RuleState::Running { port: 3306 }, stats(2, 19_293_798, 1_258_291)), "prod-web-01");
        assert_eq!(
            (card.flag, card.name.as_str(), card.type_name, card.route.as_str()),
            ("-L", "MySQL 主库", "本地转发", "127.0.0.1:3306 → 10.0.2.8:3306")
        );
        assert_eq!((card.status.as_str(), card.tone), ("运行中 · 2 个连接", Tone::Ok));
        assert_eq!(card.traffic.as_deref(), Some("↓ 18.4 MB · ↑ 1.2 MB"));
        assert!(card.retry.is_none() && card.enabled);
    }

    #[test]
    fn a_dynamic_rule_goes_out_through_the_host() {
        let rule = rule(ForwardKind::Dynamic, "1080", "");
        let card =
            card(&rule, &started(RuleState::Running { port: 1080 }, stats(7, 148_897_792, 10_276_044)), "prod-web-01");
        assert_eq!((card.flag, card.type_name), ("-D", "动态 SOCKS5"));
        assert_eq!(card.route, "127.0.0.1:1080 → 经 prod-web-01 出口");
        assert_eq!(card.traffic.as_deref(), Some("↓ 142.0 MB · ↑ 9.8 MB"));
    }

    #[test]
    fn a_remote_rule_names_the_side_of_each_address_and_shows_the_port_the_server_chose() {
        let rule = rule(ForwardKind::Remote, "127.0.0.1:0", "localhost:3000");
        assert_eq!(card(&rule, &RuleRuntime::NotStarted, "h").route, "远端 127.0.0.1:0 → 本机 localhost:3000");
        let running = card(&rule, &started(RuleState::Running { port: 38047 }, stats(0, 0, 0)), "h");
        assert_eq!(
            running.route, "远端 127.0.0.1:38047 → 本机 localhost:3000",
            "the actual port, not the 0 that was asked for"
        );
        assert_eq!(running.type_name, "远程转发");
    }

    #[test]
    fn a_rule_that_is_not_running_says_why() {
        let rule = rule(ForwardKind::Local, "3306", "db:3306");
        let text = |runtime: RuleRuntime| {
            let card = card(&rule, &runtime, "h");
            (card.status, card.tone, card.traffic)
        };
        assert_eq!(text(RuleRuntime::Disabled), ("已停用".into(), Tone::Off, None));
        assert_eq!(text(RuleRuntime::NotStarted), ("等待连接".into(), Tone::Waiting, None));
        assert_eq!(text(started(RuleState::Starting, stats(0, 0, 0))), ("正在启动…".into(), Tone::Waiting, None));
        assert_eq!(
            text(started(RuleState::WaitingReconnect, stats(0, 2048, 512))),
            ("等待重连".into(), Tone::Waiting, Some("↓ 2.0 KB · ↑ 512 B".into())),
            "what went through before the loss stays visible"
        );
        assert_eq!(
            text(started(RuleState::WaitingReconnect, stats(0, 0, 0))),
            ("等待重连".into(), Tone::Waiting, None)
        );
        assert_eq!(text(started(RuleState::Stopped, stats(0, 0, 0))).0, "已停用");
    }

    #[test]
    fn a_taken_port_offers_another_one() {
        let rule = rule(ForwardKind::Local, "8080", "10.0.3.5:3000");
        let taken = RuleError::PortInUse { host: "127.0.0.1".into(), port: 8080, suggestion: Some(18080) };
        let card = card(&rule, &started(RuleState::Failed(taken), stats(0, 0, 0)), "h");
        assert_eq!((card.status.as_str(), card.tone), ("本地端口 8080 已被占用", Tone::Failed));
        assert_eq!(
            card.retry,
            Some(RetryButton { action: RetryAction::OtherPort(18080), label: "换用 18080 重试".into() })
        );
        assert!(card.traffic.is_none());

        let no_suggestion = RuleError::PortInUse { host: "127.0.0.1".into(), port: 8080, suggestion: None };
        assert!(super::card(&rule, &started(RuleState::Failed(no_suggestion), stats(0, 0, 0)), "h").retry.is_none());
    }

    #[test]
    fn a_denied_remote_rule_names_the_server_setting_and_can_be_tried_again() {
        let rule = rule(ForwardKind::Remote, "9000", "localhost:3000");
        let card = card(&rule, &started(RuleState::Failed(RuleError::RemoteDenied), stats(0, 0, 0)), "h");
        assert_eq!(card.status, "服务器拒绝了远程转发（可能禁用了 AllowTcpForwarding）");
        assert_eq!(card.tone, Tone::Failed);
        assert_eq!(
            card.retry,
            Some(RetryButton { action: RetryAction::Again, label: "重试".into() }),
            "without toggling the rule off and on"
        );
        let other = card_for_failure(&rule, RuleError::Remote("通道已关闭".into()));
        assert_eq!(
            (other.status.as_str(), other.retry.map(|retry| retry.action)),
            ("通道已关闭", Some(RetryAction::Again))
        );
        // Other failures have nothing to retry: an incomplete rule, an address that cannot be listened on.
        assert!(card_for_failure(&rule, RuleError::MissingTarget).retry.is_none());
        let bind = RuleError::BindFailed { host: "127.0.0.1".into(), port: 80, reason: "权限不足".into() };
        assert!(card_for_failure(&rule, bind).retry.is_none());
    }

    fn card_for_failure(rule: &ForwardRule, error: RuleError) -> RuleCard {
        card(rule, &started(RuleState::Failed(error), stats(0, 0, 0)), "h")
    }

    #[test]
    fn a_port_the_server_still_holds_says_so_and_offers_to_ask_now() {
        let rule = rule(ForwardKind::Remote, "9100", "localhost:8000");
        let held = |stats| card(&rule, &started(RuleState::RemotePortHeld { port: 9100 }, stats), "h");
        let card = held(stats(0, 0, 0));
        assert_eq!(card.status, "远端端口 9100 仍被占用（可能是断线前的会话还没被服务器回收）· 正在重试");
        assert_eq!(card.tone, Tone::Waiting, "a wait, not a failure");
        assert_eq!(card.retry, Some(RetryButton { action: RetryAction::Again, label: "立即重试".into() }));
        assert_eq!(card.route, "远端 127.0.0.1:9100 → 本机 localhost:8000", "the port that was asked for");
        assert!(card.traffic.is_none(), "nothing went through yet");
        assert!(card.enabled);
        // What went through before the drop is still counted.
        assert_eq!(held(stats(0, 2048, 512)).traffic.as_deref(), Some("↓ 2.0 KB · ↑ 512 B"));
        // A rule that waits for its port does not count as running.
        let view = summary_view(
            "h",
            std::slice::from_ref(&rule),
            &[started(RuleState::RemotePortHeld { port: 9100 }, stats(0, 0, 0))],
            0,
            &[],
        );
        assert_eq!(view.summary.title, "0 条规则运行中");
    }

    #[test]
    fn the_card_carries_the_warnings_of_the_rule() {
        let lan = rule(ForwardKind::Dynamic, "0.0.0.0:1080", "");
        assert_eq!(card(&lan, &RuleRuntime::Disabled, "h").warnings, [LAN_WARNING, OPEN_PROXY_WARNING]);
        let remote = rule(ForwardKind::Remote, "0.0.0.0:9000", "x:1");
        assert_eq!(card(&remote, &RuleRuntime::Disabled, "h").warnings, [GATEWAY_HINT]);
        assert!(card(&rule(ForwardKind::Local, "3306", "x:1"), &RuleRuntime::Disabled, "h").warnings.is_empty());
    }

    #[test]
    fn the_toggle_follows_the_rule_not_the_runtime() {
        let mut rule = rule(ForwardKind::Local, "3306", "x:1");
        rule.enabled = false;
        assert!(!card(&rule, &RuleRuntime::Disabled, "h").enabled);
        rule.enabled = true;
        assert!(card(&rule, &RuleRuntime::NotStarted, "h").enabled);
    }

    #[test]
    fn replacing_the_port_keeps_the_rest_of_the_rule() {
        let original = rule(ForwardKind::Local, "8080", "10.0.3.5:3000");
        let moved = with_listen_port(&original, 18080);
        assert_eq!(moved.listen, Endpoint::new("127.0.0.1", 18080));
        assert_eq!(
            (moved.id.clone(), moved.target.clone(), moved.name.clone()),
            (original.id, original.target, original.name)
        );
    }

    // ---- the summary ----

    #[test]
    fn the_summary_counts_running_rules_and_names_the_connection() {
        let rules = [
            rule(ForwardKind::Local, "3306", "a:1"),
            rule(ForwardKind::Dynamic, "1080", ""),
            rule(ForwardKind::Local, "8080", "b:1"),
        ];
        let runtime = [
            started(RuleState::Running { port: 3306 }, stats(2, 0, 0)),
            started(RuleState::Running { port: 1080 }, stats(7, 0, 0)),
            RuleRuntime::Disabled,
        ];
        let view = summary_view("prod-web-01", &rules, &runtime, 2, &[]);
        assert_eq!(view.summary.title, "2 条规则运行中");
        assert_eq!(view.summary.subtitle, "经由 prod-web-01 的 SSH 连接 · 断线重连后自动恢复");
        assert_eq!(view.cards.len(), 3);
        assert_eq!(view.cards[2].status, "已停用");
        assert_eq!(view.rules.len(), 3);
        assert_eq!(ForwardView::empty("h").summary.title, "0 条规则运行中");
    }

    #[test]
    fn a_rule_without_a_runtime_waits_for_the_connection() {
        let rules = [rule(ForwardKind::Local, "3306", "a:1")];
        assert_eq!(summary_view("h", &rules, &[], 0, &[]).cards[0].status, "等待连接");
    }

    #[test]
    fn notes_are_passed_on_to_the_top_card() {
        let view = summary_view("h", &[], &[], 0, &[TEMPORARY_NOTE.to_string()]);
        assert_eq!(view.summary.notes, ["临时连接的规则不会保存"]);
    }

    // ---- confirmations ----

    #[test]
    fn closing_the_last_tab_warns_with_both_counts() {
        let (title, text) = close_confirmation("prod-web-01", 3, 5);
        assert_eq!(title, "关闭 prod-web-01 的最后一个标签？");
        assert_eq!(text, "关闭后 3 条端口转发会停止（当前 5 个连接会断开）");
        assert_eq!(
            close_confirmation("h", 1, 0).1,
            "关闭后 1 条端口转发会停止",
            "no connections to cut: nothing in brackets"
        );
    }

    #[test]
    fn deleting_a_rule_names_it_and_says_what_it_cuts() {
        let mut rule = rule(ForwardKind::Local, "3306", "10.0.2.8:3306");
        rule.name = "MySQL 主库".into();
        let busy = card(&rule, &started(RuleState::Running { port: 3306 }, stats(2, 0, 0)), "h");
        assert_eq!(
            delete_confirmation(&busy),
            (
                "删除规则「MySQL 主库」？".to_string(),
                "当前 2 个活动连接会被断开。\n-L  127.0.0.1:3306 → 10.0.2.8:3306".to_string()
            )
        );
        let idle = card(&rule, &RuleRuntime::Disabled, "h");
        assert_eq!(delete_confirmation(&idle).1, "-L  127.0.0.1:3306 → 10.0.2.8:3306", "nothing to cut, nothing said");
    }

    #[test]
    fn the_empty_state_explains_all_three_kinds() {
        assert_eq!(EMPTY_KINDS.map(|(name, _)| name), ["-L 本地转发", "-R 远程转发", "-D 动态转发"]);
        assert_eq!(EMPTY_KINDS.map(|(_, what)| what), ["本机端口 → 服务器侧地址", "服务器端口 → 本机服务", "本机 SOCKS5 代理"]);
    }
}
