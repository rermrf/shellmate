//! M4 scenes use the same popup, local cards and settings form as the drawer.
use crate::ai::{
    model::DrawerModel,
    tasks::{Status, TaskInfo},
};
use shellmate_ai::{ContextBreakdown, ContextBudget, ContextSnapshot, ContextWindow, UserTurn};
pub const SCENES: &[&str] = &[
    "ai-slash",
    "ai-slash-filter",
    "ai-slash-context",
    "ai-slash-tasks",
    "set-ai",
    "set-ai-advanced",
];
pub struct SettingsScene {
    pub settings: shellmate_store::ai::Settings,
    pub advanced: bool,
    pub scroll_to_bottom: bool,
}
pub fn settings_scene(scene: &str) -> Option<SettingsScene> {
    if !matches!(scene, "set-ai" | "set-ai-replace" | "set-ai-advanced") {
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
        scroll_to_bottom: advanced,
    })
}
pub struct Fixture {
    pub input: &'static str,
    pub model: DrawerModel,
    pub context: Option<ContextBreakdown>,
    pub tasks: Vec<TaskInfo>,
}
pub fn scene(scene: &str) -> Option<Fixture> {
    if !SCENES[..4].contains(&scene) {
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
    if scene == "ai-slash-context" {
        model
            .conversation
            .apply(shellmate_ai::LogUpdate::Usage(shellmate_ai::Usage {
                input_tokens: Some(36000),
                output_tokens: Some(1000),
            }));
        context = Some(
            model
                .conversation
                .breakdown_for_budget(
                    ContextBudget::new(200000, ContextWindow::Auto, 32000),
                    &shellmate_ai::agent::Capabilities {
                        terminal: true,
                        background: true,
                        ..Default::default()
                    },
                )
                .unwrap(),
        );
    }
    if scene == "ai-slash-tasks" {
        tasks = super::ai::execution_scene("ai-tasks").unwrap().tasks;
        tasks[1].status = Status::Exited(Some(0));
    }
    Some(Fixture {
        input: match scene {
            "ai-slash" => "/",
            "ai-slash-filter" => "/co",
            _ => "",
        },
        model,
        context,
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
        let mut popup = crate::ai::commands::Popup::default();
        popup.changed(scene("ai-slash").unwrap().input);
        assert!(popup.open);
        assert_eq!(popup.selected, 0);
        assert_eq!(popup.matches().len(), 6);
        popup.changed(scene("ai-slash-filter").unwrap().input);
        assert_eq!(popup.matches().len(), 2);
        let f = scene("ai-slash-context").unwrap();
        let b = f.context.unwrap();
        assert!(!f.model.conversation.entries().is_empty());
        assert_eq!(b.total, 37000);
        assert!(b.provider_usage);
        assert_eq!(b.budget.trigger, 167000);
        let tasks = scene("ai-slash-tasks").unwrap().tasks;
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].status, Status::Running);
        assert_eq!(tasks[1].status, Status::Exited(Some(0)));
    }
}
