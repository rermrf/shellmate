//! M4 scenes use the same popup, local cards and settings form as the drawer.
use crate::ai::{
    model::DrawerModel,
    tasks::{Status, TaskInfo},
};
use shellmate_ai::{ContextBreakdown, ContextBudget, ContextSnapshot, ContextWindow, UserTurn};
pub const MULTILINE_INPUT: &str = "请检查这个页面的连接问题：https://portwatch-vi.xyz/https://portwatch-vi.xyz/api/status?region=cn&service=web 并对照终端日志解释超时原因。";
pub const SCENES: &[&str] = &[
    "ai-slash",
    "ai-slash-filter",
    "ai-slash-context",
    "ai-slash-tasks",
    "ai-input-multiline",
    "ai-context-popover",
    "ai-context-amber",
    "set-ai",
    "set-ai-advanced",
];
pub struct SettingsScene {
    pub settings: shellmate_store::ai::Settings,
    pub advanced: bool,
    pub scroll_to_bottom: bool,
}
pub fn settings_scene(scene: &str) -> Option<SettingsScene> {
    if !matches!(
        scene,
        "set-ai" | "set-ai-replace" | "set-ai-advanced" | "set-ai-memory" | "set-ai-web"
    ) {
        return None;
    }
    let advanced = scene == "set-ai-advanced";
    Some(SettingsScene {
        settings: shellmate_store::ai::Settings {
            model: "gpt-5".into(),
            reasoning: shellmate_ai::ReasoningLevel::Medium,
            context_window: if advanced {
                ContextWindow::Tokens(1_000_000)
            } else {
                ContextWindow::Auto
            },
            ..Default::default()
        },
        advanced,
        scroll_to_bottom: advanced || scene == "set-ai-memory" || scene == "set-ai-web",
    })
}
pub struct Fixture {
    pub input: &'static str,
    pub model: DrawerModel,
    pub context: Option<ContextBreakdown>,
    pub context_open: bool,
    pub tasks: Vec<TaskInfo>,
}
pub fn scene(scene: &str) -> Option<Fixture> {
    if !SCENES[..7].contains(&scene) {
        return None;
    }
    let mut model = DrawerModel::default();
    let mut tasks = Vec::new();
    let mut context = None;
    if matches!(scene, "ai-slash-context" | "ai-slash-tasks") {
        model.conversation.push_user(UserTurn::new(
            "检查服务状态，保留正在运行的构建。",
            ContextSnapshot {
                label: "prod-web-01".into(),
                protocol: "SSH".into(),
                text: "deploy@prod-web-01:/srv/app$".into(),
            },
        ));
        model
            .conversation
            .push_text("服务运行正常，构建仍在后台继续。你可以查看上下文用量或管理后台任务。");
    }
    let context_scene = matches!(
        scene,
        "ai-slash-context" | "ai-context-popover" | "ai-context-amber"
    );
    if context_scene {
        model = super::ai::execution_scene("ai-output-large").unwrap().model;
        let mut entries = model.conversation.entries().to_vec();
        if let shellmate_ai::Entry::User { context, .. } = &mut entries[0] {
            context.text = (1..=40)
                .map(|n| format!("10:42:{n:02} health check /api/status returned 200"))
                .collect::<Vec<_>>()
                .join("\n");
        }
        model.conversation = shellmate_ai::Conversation::from_entries(entries);
        model.conversation.push_text(
            "构建失败来自数据库连接，服务本身仍正常运行。下一步检查测试环境的连接配置。",
        );
        let used = if scene == "ai-context-amber" {
            155_000
        } else {
            73_600
        };
        model
            .conversation
            .apply(shellmate_ai::LogUpdate::Usage(shellmate_ai::Usage {
                input_tokens: Some(used - 1000),
                output_tokens: Some(1000),
            }));
        // Pin the pre-removal demo's accounting so prompt-rule edits do not shift its pixels.
        let mut breakdown = ContextBreakdown {
            system_prompt: 704,
            instructions_memory: 0,
            tool_definitions: 861,
            tool_count: 4,
            terminal_context: 597,
            summary: 0,
            messages: 130,
            tool_results: 212,
            total: 2504,
            provider_usage: true,
            budget: ContextBudget::new(200000, ContextWindow::Auto, 32000),
        };
        breakdown.scale_to(used as usize);
        context = Some(breakdown);
    }
    if scene == "ai-slash-tasks" {
        tasks = super::ai::execution_scene("ai-tasks").unwrap().tasks;
        tasks[1].status = Status::Exited(Some(0));
    }
    Some(Fixture {
        input: match scene {
            "ai-slash" => "/",
            "ai-slash-filter" => "/co",
            "ai-input-multiline" => MULTILINE_INPUT,
            _ => "",
        },
        model,
        context,
        context_open: context_scene && scene != "ai-context-amber",
        tasks,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_fixtures_use_auto_defaults_and_expanded_custom_window() {
        let fixture = settings_scene("set-ai").unwrap();
        assert!(!fixture.advanced);
        assert!(!fixture.scroll_to_bottom);
        let main = fixture.settings;
        assert_eq!(main.context_window, ContextWindow::Auto);
        assert_eq!(main.autocompact_window, ContextWindow::Auto);
        assert_eq!(main.max_output_tokens, shellmate_ai::OutputTokens::Auto);
        assert_eq!(main.max_steps, None);
        assert_eq!(main.exec, shellmate_ai::agent::ExecLimits::default());
        let fixture = settings_scene("set-ai-advanced").unwrap();
        assert!(fixture.advanced);
        assert!(fixture.scroll_to_bottom);
        let custom = fixture.settings;
        assert_eq!(custom.context_window, ContextWindow::Tokens(1000 * 1000));
        assert_eq!(custom.context_budget().trigger, 967000);
    }
    #[test]
    fn scenes_register_both_themes_default_and_small_sizes() {
        for scene in SCENES {
            assert!(super::super::SCENES.contains(scene));
            assert_eq!(super::super::default_window_size(scene), (1440, 900));
            for dark in [false, true] {
                assert!(
                    super::super::readings::READINGS
                        .iter()
                        .any(|r| r.scene == *scene && r.dark == dark)
                );
            }
            assert_eq!(
                super::super::request(&format!("{scene}@1100x700")),
                (*scene, Some((1100, 700)))
            );
        }
    }
    #[test]
    fn fixtures_open_filter_context_and_finished_tasks() {
        let multiline = scene("ai-input-multiline").unwrap();
        assert_eq!(multiline.input, MULTILINE_INPUT);
        assert!(multiline.input.contains("请检查"));
        assert!(
            multiline
                .input
                .contains("https://portwatch-vi.xyz/https://")
        );
        assert!(
            !multiline.input.contains('\n'),
            "fixture exercises soft wrapping of a URL"
        );
        assert!(multiline.model.conversation.entries().is_empty());
        let mut popup = crate::ai::commands::Popup::default();
        popup.changed(scene("ai-slash").unwrap().input);
        assert!(popup.open);
        assert_eq!(popup.selected, 0);
        assert_eq!(popup.matches().len(), 8);
        popup.changed(scene("ai-slash-filter").unwrap().input);
        assert_eq!(popup.matches().len(), 2);
        let f = scene("ai-slash-context").unwrap();
        let b = f.context.unwrap();
        assert!(!f.model.conversation.entries().is_empty());
        assert_eq!(b.total, 73600);
        assert_eq!(
            (
                b.system_prompt,
                b.tool_definitions,
                b.terminal_context,
                b.messages,
                b.tool_results
            ),
            (20692, 25308, 17547, 3821, 6232)
        );
        assert!(f.context_open);
        assert!(b.terminal_context > 0 && b.tool_results > 0 && b.tool_definitions > 0);
        assert!(b.provider_usage);
        assert_eq!(b.budget.trigger, 167000);
        let amber = scene("ai-context-amber").unwrap();
        assert!(!amber.context_open);
        let b = amber.context.unwrap();
        assert!(
            crate::ai::model::ContextMeter {
                used: b.total,
                window: b.budget.window,
                trigger: b.budget.trigger
            }
            .amber()
        );
        let normal = scene("ai-context-popover").unwrap().context.unwrap();
        assert_eq!(
            crate::ai::context_view::percent(normal.total, normal.budget.window),
            "37%"
        );
        assert!(
            !crate::ai::model::ContextMeter {
                used: normal.total,
                window: normal.budget.window,
                trigger: normal.budget.trigger
            }
            .amber()
        );
        let tasks = scene("ai-slash-tasks").unwrap().tasks;
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].status, Status::Running);
        assert_eq!(tasks[1].status, Status::Exited(Some(0)));
    }
}
