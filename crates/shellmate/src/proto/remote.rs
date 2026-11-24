//! M2 fixtures: the real trust card and quiet context projection, no discovery I/O.
use shellmate_core::remote_instructions::{Discovery, Document};
pub const SCENES: &[&str] = &["ai-remote-trust", "ai-remote-loaded"];
pub fn discovery() -> Discovery {
    Discovery {
        cancelled: false,
        cwd: "/srv/app".into(),
        files: vec![
            Document {
                path: "/srv/AGENTS.md".into(),
                text: "# 服务器项目约定\n部署前先检查服务健康状态。\n优先使用只读命令排查。".into(),
                bytes: 112,
                truncated: false,
            },
            Document {
                path: "/srv/app/CLAUDE.md".into(),
                text: "# 应用说明\n使用 Docker Compose 管理服务。\n变更前确认当前部署版本。".into(),
                bytes: 106,
                truncated: false,
            },
        ],
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scenes_have_both_themes_and_document_choices() {
        for scene in SCENES {
            assert!(super::super::SCENES.contains(scene));
            for dark in [false, true] {
                assert!(
                    super::super::readings::READINGS
                        .iter()
                        .any(|r| r.scene == *scene && r.dark == dark)
                );
            }
        }
        assert_eq!(discovery().files.len(), 2);
        assert_eq!(discovery().files[0].path, "/srv/AGENTS.md");
    }
}
