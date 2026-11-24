//! Permission popup scene: full access selected, no model or command execution.
pub const SCENE: &str = "ai-mode-popover";
pub fn conversation() -> shellmate_ai::Conversation {
    use shellmate_ai::{ContextSnapshot, Conversation, UserTurn};
    let mut conversation = Conversation::default();
    conversation.push_user(UserTurn::new(
        "检查服务状态",
        ContextSnapshot {
            label: "prod-web-01".into(),
            protocol: "SSH".into(),
            text: "deploy@prod-web-01:/srv/app$".into(),
        },
    ));
    conversation
        .push_text("服务运行正常，最近没有异常日志。你可以继续排查，或调整此主机的审批模式。");
    conversation
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_is_registered_for_both_themes_and_standard_sizes() {
        assert!(super::super::SCENES.contains(&SCENE));
        assert_eq!(super::super::default_window_size(SCENE), (1440, 900));
        assert_eq!(
            super::super::request("ai-mode-popover@1100x700"),
            (SCENE, Some((1100, 700)))
        );
        for dark in [false, true] {
            assert!(
                super::super::readings::READINGS
                    .iter()
                    .any(|r| r.scene == SCENE && r.dark == dark)
            );
        }
        assert_eq!(conversation().turns().len(), 1);
    }
}
