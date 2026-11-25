//! Output fixtures use a private temp root, including real spool metadata and retained snapshots.
use shellmate_ai::{
    self as ai, Entry, LoggedCall, LoggedResult, Step, StepDisplay, StepStatus, Target,
};
use shellmate_core::ai_output::Spool;
pub const SCENES: &[&str] = &[
    "ai-output-short",
    "ai-output-expanded",
    "ai-output-history",
    "ai-output-missing",
    "ai-output-retained",
    "ai-output-running",
    "ai-output-giant",
];
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub conversation: ai::Conversation,
    pub expanded: bool,
    pub readonly: bool,
    pub spool: Option<Spool>,
    pub tasks: Vec<crate::ai::tasks::TaskInfo>,
}
pub fn scene(name: &str, conversation: &str) -> Option<Fixture> {
    if !SCENES.contains(&name) {
        return None;
    }
    let root = tempfile::tempdir().expect("output fixture root");
    let mut text = if name == "ai-output-short" {
        "服务运行正常\n输出 id literal-user-uuid\n".into()
    } else {
        (0..3500)
            .map(|i| {
                format!(
                    "{i:04} · 检查配置与依赖 build-step-{i} {}\n",
                    if i == 1800 { "MIDDLE-EVIDENCE" } else { "ok" }
                )
            })
            .collect::<String>()
            + "FINAL-EVIDENCE\n"
    };
    if name == "ai-output-giant" {
        text = format!("{}END-OF-GIANT-LINE", "x".repeat(10 * 1024 * 1024));
    }
    let spool = Spool::create(root.path(), conversation, "fixture-output").unwrap();
    spool.write(text.as_bytes(), false).unwrap();
    let running = name == "ai-output-running";
    if !running {
        spool.finish().unwrap();
    }
    if name == "ai-output-retained" {
        let path = spool.path();
        let retained = "HEAD-EVIDENCE\n[省略中间输出；仅保留首尾]\nTAIL-EVIDENCE\n";
        std::fs::write(&path, retained).unwrap();
        std::fs::write(
            path.with_extension("meta"),
            format!("70000000 {} 0 0\n", retained.len()),
        )
        .unwrap();
    }
    let out = ai::output::spooled(
        "fixture-output",
        "cargo build",
        text.as_bytes(),
        text.len() as u64,
        Some(0),
        std::time::Duration::from_secs(4),
        Default::default(),
    );
    let mut log = ai::Conversation::default();
    log.push_user(ai::UserTurn::new("检查部署输出", Default::default()));
    log.append(Entry::Step(Step {
        message_id: 1,
        call: LoggedCall {
            id: "fixture-call".into(),
            name: "run_command".into(),
            args_json: serde_json::json!({"command":"cargo build","target":"background"}),
        },
        result: LoggedResult {
            inline: out.wire_text(),
            output_id: Some("fixture-output".into()),
            is_error: false,
            cleared: false,
            subagent: None,
        },
        display: Some(StepDisplay {
            command: "cargo build --release".into(),
            target: Target::Background,
            purpose: "检查构建输出，确认完整日志可查看和复制".into(),
            class: ai::policy::Class::ReadOnly,
            status: if running {
                StepStatus::Background
            } else {
                StepStatus::Completed
            },
            exit_code: Some(0),
            duration_ms: 4500,
            automatic: true,
            output: out.stored(),
        }),
    }));
    let tasks = if running {
        vec![crate::ai::tasks::TaskInfo {
            kind: crate::ai::tasks::Kind::Command,
            call_id: "fixture-call".into(),
            id: "1".into(),
            command: "cargo build --release".into(),
            target: Target::Background,
            started: std::time::Instant::now(),
            status: crate::ai::tasks::Status::Running,
            output_id: "fixture-output".into(),
            background: true,
        }]
    } else {
        vec![]
    };
    if name == "ai-output-missing" {
        std::fs::remove_file(spool.path()).unwrap();
    }
    Some(Fixture {
        root,
        conversation: log,
        expanded: name != "ai-output-short",
        readonly: name == "ai-output-history",
        spool: running.then_some(spool),
        tasks,
    })
}
