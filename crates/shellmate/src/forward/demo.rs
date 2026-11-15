//! Debug-only review data: cards made up for the screenshots of every state the panel can be in,
//! without a server (`SHELLMATE_DEMO=forward:<host>:<set>`, see `demo::run`).

use shellmate_forward::{ForwardKind, ForwardRule, RuleError, RuleId, RuleState, StatsSnapshot};

use super::model::{ForwardView, RuleRuntime, TEMPORARY_NOTE, summary_view};

fn rule(name: &str, kind: ForwardKind, listen: &str, target: &str) -> ForwardRule {
    ForwardRule::parse(RuleId::generate(), name, kind, listen, target, true).expect("a fixture rule")
}

fn running(port: u16, active: u32, down: u64, up: u64) -> RuleRuntime {
    RuleRuntime::Started { state: RuleState::Running { port }, stats: StatsSnapshot { active, down, up } }
}

fn failed(error: RuleError) -> RuleRuntime {
    RuleRuntime::Started { state: RuleState::Failed(error), stats: StatsSnapshot::default() }
}

/// The three rules of the prototype's screenshot.
fn prototype() -> (Vec<ForwardRule>, Vec<RuleRuntime>) {
    (
        vec![
            rule("MySQL 主库", ForwardKind::Local, "127.0.0.1:3306", "10.0.2.8:3306"),
            rule("SOCKS5 代理", ForwardKind::Dynamic, "127.0.0.1:1080", ""),
            rule("Grafana", ForwardKind::Local, "127.0.0.1:8080", "10.0.3.5:3000"),
        ],
        vec![
            running(3306, 2, 19_293_798, 1_258_291),
            running(1080, 7, 148_897_792, 10_276_044),
            failed(RuleError::PortInUse { host: "127.0.0.1".into(), port: 8080, suggestion: Some(18080) }),
        ],
    )
}

/// Every state in one list.
fn states() -> (Vec<ForwardRule>, Vec<RuleRuntime>) {
    let mut disabled = rule("预发环境数据库", ForwardKind::Local, "127.0.0.1:5433", "staging-db.internal:5432");
    disabled.enabled = false;
    (
        vec![
            rule("MySQL 主库", ForwardKind::Local, "127.0.0.1:3306", "10.0.2.8:3306"),
            rule("回调测试", ForwardKind::Remote, "127.0.0.1:0", "localhost:3000"),
            rule("SOCKS5 代理", ForwardKind::Dynamic, "127.0.0.1:1080", ""),
            rule("Grafana", ForwardKind::Local, "127.0.0.1:8080", "10.0.3.5:3000"),
            rule("Webhook", ForwardKind::Remote, "9000", "localhost:3000"),
            rule("局域网共享代理", ForwardKind::Dynamic, "0.0.0.0:1081", ""),
            rule("内网文档站", ForwardKind::Remote, "0.0.0.0:9100", "localhost:8000"),
            disabled,
            rule("新加的规则", ForwardKind::Local, "127.0.0.1:6379", "redis.internal:6379"),
        ],
        vec![
            running(3306, 2, 19_293_798, 1_258_291),
            running(38047, 1, 4_096, 812),
            RuleRuntime::Started {
                state: RuleState::WaitingReconnect,
                stats: StatsSnapshot { active: 0, down: 148_897_792, up: 10_276_044 },
            },
            failed(RuleError::PortInUse { host: "127.0.0.1".into(), port: 8080, suggestion: Some(18080) }),
            failed(RuleError::RemoteDenied),
            running(1081, 0, 0, 0),
            RuleRuntime::Started { state: RuleState::Starting, stats: StatsSnapshot::default() },
            RuleRuntime::Disabled,
            RuleRuntime::NotStarted,
        ],
    )
}

/// The new card state next to a running rule and a plain refusal: the server still holds the port of a remote
/// rule that worked before, and a rule the server never granted.
fn held() -> (Vec<ForwardRule>, Vec<RuleRuntime>) {
    (
        vec![
            rule("MySQL 主库", ForwardKind::Local, "127.0.0.1:3306", "10.0.2.8:3306"),
            rule("回显服务", ForwardKind::Remote, "127.0.0.1:9100", "localhost:8000"),
            rule("Webhook", ForwardKind::Remote, "9000", "localhost:3000"),
        ],
        vec![
            running(3306, 2, 19_293_798, 1_258_291),
            RuleRuntime::Started {
                state: RuleState::RemotePortHeld { port: 9100 },
                stats: StatsSnapshot { active: 0, down: 4_096, up: 812 },
            },
            failed(RuleError::RemoteDenied),
        ],
    )
}

/// The view for the set called `name` on the connection to `host`: `prototype` (the three rules of the
/// prototype), `states` (every state), `held` (a remote port the server still holds), `temporary` (the prototype's, for a host that is not saved), or
/// `empty`.
pub fn view(name: &str, host: &str) -> ForwardView {
    let (rules, runtime, notes) = match name {
        "empty" => (Vec::new(), Vec::new(), Vec::new()),
        "states" => {
            let (rules, runtime) = states();
            (rules, runtime, Vec::new())
        }
        "held" => {
            let (rules, runtime) = held();
            (rules, runtime, Vec::new())
        }
        "temporary" => {
            let (rules, runtime) = prototype();
            (rules, runtime, vec![TEMPORARY_NOTE.to_string()])
        }
        _ => {
            let (rules, runtime) = prototype();
            (rules, runtime, Vec::new())
        }
    };
    let running = runtime
        .iter()
        .filter(|runtime| matches!(runtime, RuleRuntime::Started { state: RuleState::Running { .. }, .. }))
        .count();
    summary_view(host, &rules, &runtime, running, &notes)
}
