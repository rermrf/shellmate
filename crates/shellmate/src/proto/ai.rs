//! Static answer sources shared by the real drawer and headless fixture checks.
pub const AGENT_ANALYSIS: &str = r#"检查完成，容器服务运行正常。

- **运行状态：** 5 个容器均在运行，使用 `docker ps` 已确认。
- **入口日志：** `nginx` 最近请求正常，没有发现 `5xx`。
- **内部服务：** API、数据库和缓存通过内部网络提供服务。

| 名称 | 镜像 | 端口 |
| --- | --- | --- |
| nginx | nginx:1.27-alpine | 443/tcp |
| api | shellmate/api:2.8.1 | 8080/tcp |
| worker | shellmate/jobs:2.8.1 | — |
| postgres | postgres:16-alpine | 5432/tcp |
| redis | redis:7-alpine | 6379/tcp |

如需继续查看资源占用：

```sh
docker stats --no-stream
```"#;

pub const CHAT_ANALYSIS: &str = "不会，这是一条只读查询命令。\n\n1. `lsof` 只列出监听 `3000` 端口的进程及 PID。\n2. 先预览命令，再确认目标会话为 `prod-web-01`。\n3. 将结果发给我后，可以继续分析端口冲突。";

pub const MARKDOWN: &str = r#"## 服务检查结果

当前服务**运行正常**，建议*持续观察*；可查阅 [Docker 文档](https://docs.docker.com/)。

### 接下来检查什么

- 入口服务 `nginx`
  - 检查最近的访问日志
  - 确认 `443` 端口与证书有效期
- 内部服务
  - 数据库 `postgres` 与缓存 `redis`

> 以下命令只读取状态。执行前请确认当前会话和目标容器。

```sh
docker inspect nginx --format '{{.Name}} {{.Config.Image}} {{json .NetworkSettings.Ports}} {{json .NetworkSettings.Networks}} {{json .Mounts}}'
```

| 名称 | 镜像版本 | 端口映射 | 挂载目录 | 健康状态 |
| --- | --- | --- | --- | --- |
| nginx | registry.example.com/nginx:1.27 | 443/tcp | /srv/nginx | 正常 |
| api | registry.example.com/api:2.8.1 | 8080/tcp | /srv/api | 正常 |

![容器服务拓扑](https://example.invalid/never-fetch.png)
"#;

pub const COMPACTION_SCENES: &[&str] = &[
    "ai-compacted", "ai-compacted-open", "ai-compacting", "ai-compacted-tier1",
];

pub const COMPACTION_SUMMARY: &str = "1. 主要请求与意图：定位 502，只读检查。\n2. 关键概念与背景：nginx 转发至 api:8080。\n3. 涉及的文件与内容：待查 app.conf，未修改。\n4. 错误与修复：上游超时；尚未实施修复。\n5. 解决过程：已确认容器运行，待查响应时延。\n6. 用户消息：保留排查记录，不直接改配置。\n7. 待办事项：检查健康端点和超时设置。\n8. 当前工作：已核对容器状态与入口日志。\n9. 下一步：验证 API 健康端点，对照同期日志。";

pub struct CompactionScene {
    pub model: crate::ai::model::DrawerModel,
    pub expanded_summary: Option<(usize, usize)>,
}

/// The same semantic log and model consumed by the live drawer, with inert data only.
pub fn compaction_scene(scene: &str) -> Option<CompactionScene> {
    use shellmate_ai::{
        CompactTrigger, Compaction, ContextSnapshot, Conversation, Entry, Event,
        LoggedCall, LoggedResult, PermissionMode, RetainedState, Step, StepDisplay,
        StepStatus, Target, UserTurn, policy::Class,
    };
    if !COMPACTION_SCENES.contains(&scene) { return None; }
    let context = ContextSnapshot {
        label: "prod-web-01".into(),
        protocol: "SSH".into(),
        text: "root@prod-web-01:/srv/app#".into(),
    };
    let mut conversation = Conversation::default();
    conversation.push_user(UserTurn::new("检查 nginx 的 502 报错，先不要修改配置。", context.clone()));
    for (n, command, purpose, output, cleared) in [
        (1, "docker ps --format 'table {{.Names}}\t{{.Status}}'", "确认容器运行状态",
         "标准输出：\nnginx    Up 12 days\napi      Up 3 days\n标准错误：\n", true),
        (2, "docker logs --tail 3 nginx", "检查入口服务最近的报错",
         "标准输出：\n10:42:03 upstream timed out: api:8080\n标准错误：\n", false),
    ] {
        conversation.append(Entry::Step(Step {
            message_id: n,
            call: LoggedCall {
                id: format!("fixture_context_{n}"),
                name: "run_command".into(),
                args_json: serde_json::json!({"command":command,"purpose":purpose,"target":"background"}),
            },
            result: LoggedResult { inline: output.into(), output_id: None, cleared, is_error: false },
            display: Some(StepDisplay {
                command: command.into(), target: Target::Background, purpose: purpose.into(),
                class: Class::ReadOnly, status: StepStatus::Completed, exit_code: Some(0),
                duration_ms: 1200, automatic: true, output: output.into(),
            }),
        }));
    }
    let mut model = crate::ai::model::DrawerModel { conversation, ..Default::default() };
    let mut expanded_summary = None;
    if scene == "ai-compacting" {
        model.event(Event::Context { used: 93_000, window: 128_000, trigger: 95_000 });
        model.event(Event::Compacting { tier: 2 });
    } else {
        let key = (0, model.conversation.turns()[0].answer.as_ref().unwrap().items.len());
        model.conversation.append(Entry::Compaction(Compaction {
            summary: (scene != "ai-compacted-tier1").then(|| COMPACTION_SUMMARY.into()),
            retained_user: vec!["检查 nginx 的 502 报错，先不要修改配置。".into()],
            state: RetainedState { permission_mode: PermissionMode::Auto, ..Default::default() },
            context: context.clone(), before_tokens: 812_000, after_tokens: 41_000,
            trigger: CompactTrigger::PreRequest,
        }));
        if scene == "ai-compacted-open" { expanded_summary = Some(key); }
        model.conversation.push_user(UserTurn::new("下一步先确认 API 是否能响应。", context));
        model.conversation.push_text("先检查 API 健康端点与响应耗时，再对照超时日志。当前未修改配置或重启服务。");
        model.event(Event::Context { used: 41_000, window: 1_000_000, trigger: 967_000 });
    }
    Some(CompactionScene { model, expanded_summary })
}

#[cfg(test)]
mod tests {
    use markdown::{ParseOptions, mdast::Node};

    fn walk<'a>(node: &'a Node, nodes: &mut Vec<&'a Node>) {
        nodes.push(node);
        if let Some(children) = node.children() {
            for child in children {
                walk(child, nodes);
            }
        }
    }

    #[test]
    fn ai_markdown_fixture_covers_requested_constructs() {
        let root = markdown::to_mdast(super::MARKDOWN, &ParseOptions::gfm()).unwrap();
        let mut nodes = Vec::new();
        walk(&root, &mut nodes);
        for depth in [2, 3] {
            assert!(
                nodes
                    .iter()
                    .any(|n| matches!(n, Node::Heading(h) if h.depth == depth))
            );
        }
        assert!(nodes.iter().any(|n| matches!(n, Node::Strong(_))));
        assert!(nodes.iter().any(|n| matches!(n, Node::Emphasis(_))));
        assert!(nodes.iter().any(|n| matches!(n, Node::Link(_))));
        assert!(nodes.iter().any(|n| matches!(n, Node::Blockquote(_))));
        assert!(nodes.iter().any(|n| matches!(n, Node::Image(_))));
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, Node::Code(c) if c.value.len() > 120))
        );
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, Node::Table(t) if t.align.len() == 5))
        );
        assert!(nodes.iter().filter(|n| matches!(n, Node::List(_))).count() >= 3);
        let agent = markdown::to_mdast(super::AGENT_ANALYSIS, &ParseOptions::gfm()).unwrap();
        nodes.clear();
        walk(&agent, &mut nodes);
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, Node::Table(t) if t.align.len() == 3))
        );
        let chat = markdown::to_mdast(super::CHAT_ANALYSIS, &ParseOptions::gfm()).unwrap();
        nodes.clear();
        walk(&chat, &mut nodes);
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, Node::List(l) if l.ordered))
        );
        assert!(nodes.iter().any(|n| matches!(n, Node::InlineCode(_))));
    }

    #[test]
    fn ai_fixtures_have_both_themes_and_requested_sizes() {
        assert_eq!(super::super::default_window_size("ai-new-confirm"), (1440, 900));
        for scene in ["ai", "ai-chat", "ai-agent-run", "ai-new-confirm", "ai-markdown", "ai-markdown-wide"]
            .into_iter().chain(super::COMPACTION_SCENES.iter().copied()).chain(super::EXECUTION_SCENES.iter().copied())
        {
            assert!(super::super::SCENES.contains(&scene));
            assert_eq!(super::super::default_window_size(scene), super::super::default_window_size("ai-chat"));
            for dark in [false, true] {
                assert!(
                    super::super::readings::READINGS
                        .iter()
                        .any(|r| r.scene == scene && r.dark == dark)
                );
            }
            for (w, h) in [(1440, 900), (1100, 700)] {
                let request = format!("{scene}@{w}x{h}");
                assert_eq!(super::super::request(&request), (scene, Some((w, h))));
            }
        }
    }

    #[test]
    fn compaction_scenes_cover_real_log_dividers_expansion_and_busy_meter() {
        use shellmate_ai::{AnswerItem, Entry};
        let collapsed = super::compaction_scene("ai-compacted").unwrap();
        let open = super::compaction_scene("ai-compacted-open").unwrap();
        assert!(collapsed.model.conversation == open.model.conversation);
        assert!(collapsed.expanded_summary.is_none());
        assert_eq!(open.expanded_summary, Some((0, 2)));
        assert!(super::compaction_scene("ai-chat").is_none());
        for scene in super::COMPACTION_SCENES {
            let fixture = super::compaction_scene(scene).unwrap();
            let model = &fixture.model;
            let entries = model.conversation.entries();
            assert!(matches!(&entries[..3], [Entry::User { .. }, Entry::Step(first), Entry::Step(second)]
                if first.result.cleared && !second.result.cleared));
            let meter = model.meter.unwrap();
            if *scene == "ai-compacting" {
                assert!(model.busy && model.compacting && meter.amber());
                assert_eq!((meter.used,meter.window), (93_000,128_000));
                assert_eq!(model.status(false), "正在压缩上下文…");
                assert!(!entries.iter().any(|e| matches!(e, Entry::Compaction(_))));
            } else {
                assert!(!model.busy && !model.compacting && !meter.amber());
                assert_eq!((meter.used,meter.window), (41_000,1_000_000));
                assert!(matches!(&entries[3..], [Entry::Compaction(_), Entry::User { .. }, Entry::Assistant { .. }]));
                let answer = model.conversation.turns()[0].answer.as_ref().unwrap();
                let [AnswerItem::Step { .. }, AnswerItem::Step { .. }, AnswerItem::Compaction(c)] = &answer.items[..] else { panic!("expected steps then divider"); };
                assert_eq!(crate::ai::model::compaction_label(c), "已压缩上下文 · 812K → 41K");
                if *scene == "ai-compacted-tier1" {
                    assert!(c.summary.is_none(), "tier 1 has no summary toggle");
                    assert!(fixture.expanded_summary.is_none());
                } else {
                    let summary = c.summary.as_deref().unwrap();
                    assert_eq!(summary, super::COMPACTION_SUMMARY);
                    assert!(summary.lines().count() >= 5);
                    for section in ["1. 主要请求与意图", "3. 涉及的文件与内容", "4. 错误与修复", "7. 待办事项", "9. 下一步"] {
                        assert!(summary.contains(section));
                    }
                }
            }
        }
    }

}

pub const EXECUTION_SCENES: &[&str] = &["ai-tasks", "ai-output-large"];
pub struct ExecutionScene {
    pub model: crate::ai::model::DrawerModel,
    pub tasks: Vec<crate::ai::tasks::TaskInfo>,
}
pub fn execution_scene(scene: &str) -> Option<ExecutionScene> {
    use crate::ai::tasks::{Status, TaskInfo};
    use shellmate_ai::{
        ContextSnapshot, Entry, LoggedCall, LoggedResult, Step, StepDisplay, StepStatus, Target,
        UserTurn, policy::Class,
    };
    if !EXECUTION_SCENES.contains(&scene) {
        return None;
    }
    let mut model = crate::ai::model::DrawerModel::default();
    model.conversation.push_user(UserTurn::new(
        if scene == "ai-tasks" {
            "构建服务并在终端运行集成测试。"
        } else {
            "检查构建日志，找出失败原因。"
        },
        ContextSnapshot {
            label: "prod-web-01".into(),
            protocol: "SSH".into(),
            text: String::new(),
        },
    ));
    let step = |n, command: &str, output: &str, code: Option<i32>| {
        Entry::Step(Step {
            message_id: n,
            call: LoggedCall {
                id: format!("fixture_exec_{n}"),
                name: "run_command".into(),
                args_json: serde_json::json!({"command":command,"target":"background"}),
            },
            result: LoggedResult {
                inline: output.into(),
                output_id: Some(format!("output-{n}")),
                cleared: false,
                is_error: code.is_some_and(|n: i32| n != 0),
            },
            display: Some(StepDisplay {
                command: command.into(),
                target: Target::Background,
                purpose: "检查构建与测试输出".into(),
                class: Class::Other,
                status: if code.is_none() {
                    StepStatus::Background
                } else {
                    StepStatus::Completed
                },
                exit_code: code,
                duration_ms: 120000,
                automatic: false,
                output: output.into(),
            }),
        })
    };
    let mut tasks = Vec::new();
    if scene == "ai-tasks" {
        model.conversation.append(step(1, "cd /srv/app && cargo build --release", "命令在 120 秒内未完成，已转到后台继续运行 · 任务 #1 · 输出 id output-1 · 其中的 cd 不会影响后续命令", None));
        model
            .conversation
            .push_text("构建和集成测试仍在运行，输出已保存，可以随时查看。  ");
        for (id, command, target, elapsed) in [
            (
                "1",
                "cd /srv/app && cargo build --release",
                Target::Background,
                147,
            ),
            (
                "2",
                "cargo test --test integration -- --nocapture",
                Target::Terminal,
                36,
            ),
        ] {
            tasks.push(TaskInfo {
                call_id: format!("fixture-{id}"),
                id: id.into(),
                command: command.into(),
                target,
                started: std::time::Instant::now() - std::time::Duration::from_secs(elapsed),
                status: Status::Running,
                output_id: format!("output-{id}"),
                background: true,
            });
        }
    } else {
        model.conversation.append(step(1, "cat /srv/app/build.log", "输出 id output-1 · 总计 1048576 字节 · 退出码 0\n[build] Compiling api v2.8.1\n[build] Checking dependencies…\n[build] Running release build…\n完整输出已保存，用 read_output 查看或搜索", Some(0)));
        model.conversation.append(Entry::Step(Step {
            message_id: 2,
            call: LoggedCall {
                id: "fixture_read".into(),
                name: "read_output".into(),
                args_json: serde_json::json!({"id":"output-1","grep":"error"}),
            },
            result: LoggedResult {
                inline: "error: connection refused".into(),
                output_id: Some("output-1".into()),
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
        model.conversation.append(step(3, "cargo test --test integration", "输出 id output-3 · 总计 86024 字节 · 退出码 101\nrunning 42 tests\ntest health_check ... ok\n[省略中间输出]\n[stderr] error: connection refused at localhost:5432\ntest database_connection ... FAILED\ntest result: FAILED. 41 passed; 1 failed", Some(101)));
    }
    Some(ExecutionScene { model, tasks })
}

#[cfg(test)]
mod execution_tests {
    #[test]
    fn scenes_use_real_steps_and_quiet_output_projection() {
        use shellmate_ai::{AnswerItem, Entry};
        let tasks = super::execution_scene("ai-tasks").unwrap();
        assert_eq!(tasks.tasks.len(), 2);
        assert_ne!(tasks.tasks[0].target, tasks.tasks[1].target);
        assert!(
            tasks
                .tasks
                .iter()
                .all(|t| t.background && t.status == crate::ai::tasks::Status::Running)
        );
        assert!(
            tasks
                .model
                .conversation
                .entries()
                .iter()
                .any(|e| matches!(e, Entry::Step(s) if s.result.inline.contains("已转到后台")))
        );
        let output = super::execution_scene("ai-output-large").unwrap();
        let items = &output.model.conversation.turns()[0]
            .answer
            .as_ref()
            .unwrap()
            .items;
        if let AnswerItem::Step { output, .. } = &items[0] {
            let view = crate::ai::output_view::display(output);
            assert_eq!(view.saved.as_deref(), Some("完整输出 1.0 MB 已保存 · output-1"));
            assert!(!view.preview.contains("输出 id"));
            assert!(!view.preview.contains("read_output"));
            assert!(output.starts_with("输出 id output-1 · 总计 1048576 字节"));
        } else { panic!("large-output step"); }
        assert!(matches!(&items[1], AnswerItem::Notice(text) if text.contains("读取输出")));
        assert!(
            matches!(&items[2], AnswerItem::Step { output, exit_code: Some(101), .. } if output.contains("省略中间输出") && output.contains("FAILED"))
        );
    }
}


pub const COPY_SCENES: &[&str] = &["ai-code-copy", "ai-code-copied", "ai-code-select"];
pub const COPY_CODE: &str = "docker compose -f deploy/docker-compose.production.yml --project-name shellmate-prod --profile workers --env-file deploy/production.env up -d --remove-orphans api worker nginx";
pub fn copy_answer() -> String {
    format!("可以先核对启动参数，再按需执行：\n\n```sh\n{COPY_CODE}\n```\n\n这条命令使用生产环境配置，启动 API、后台任务与入口服务。")
}
#[cfg(test)]
mod copy_tests {
    #[test]
    fn copy_scenes_have_long_code_both_themes_and_standard_sizes() {
        for scene in super::COPY_SCENES {
            assert!(super::super::SCENES.contains(scene));
            assert_eq!(super::super::default_window_size(scene),(1440,900));
            assert_eq!(super::super::request(&format!("{scene}@1100x700")),(*scene,Some((1100,700))));
            for dark in [false,true] {assert!(super::super::readings::READINGS.iter().any(|r|r.scene==*scene && r.dark==dark));}
        }
        let parsed=markdown::to_mdast(&super::copy_answer(),&markdown::ParseOptions::gfm()).unwrap();
        assert!(parsed.children().unwrap().iter().any(|n|matches!(n,markdown::mdast::Node::Code(code) if code.value==super::COPY_CODE)));
        assert!(super::COPY_CODE.len()>150);
    }
}
