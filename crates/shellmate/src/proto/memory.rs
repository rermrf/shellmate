//! P12 memory fixtures populate only the prototype's temporary data root.
use shellmate_core::memory::Memory;
use std::path::Path;
pub const SCENES: &[&str] = &[
    "set-ai-memory",
    "memory-manager",
    "memory-manager-empty",
    "memory-manager-edit",
    "memory-manager-agents",
    "memory-manager-new",
    "host-editor-ai-note",
    "ai-memory-chip",
];
pub const HOST_NOTE: &str = "部署前先检查服务健康状态。\n生产环境变更请说明影响范围，再执行操作。\n所有部署使用 /srv/app 下的 Docker Compose 配置。";
pub fn seed(root: &Path, host: &str) {
    let memory = Memory::new(root, Some(host));
    memory.save("/memories/host/AGENTS.md", HOST_NOTE).unwrap();
    for (file, name, description, kind) in [
        (
            "deploy.md",
            "部署约定",
            "应用位于 /srv/app，使用 Docker Compose",
            "project",
        ),
        (
            "feedback.md",
            "排障反馈",
            "先读取日志，再提出变更建议",
            "feedback",
        ),
        (
            "dashboard.md",
            "监控入口",
            "生产监控与工单的资料位置",
            "reference",
        ),
    ] {
        memory.save(&format!("/memories/host/{file}"),&format!("---\nname: {name}\ndescription: {description}\ntype: {kind}\n---\n\n{description}。\n\n执行变更前先核实当前服务状态。\n")).unwrap();
    }
    memory.save("/memories/host/MEMORY.md","- [部署约定](deploy.md) — Docker Compose 部署方式\n- [排障反馈](feedback.md) — 先读日志\n- [监控入口](dashboard.md) — 外部参考资料\n").unwrap();
    memory.save("/memories/global/deploy.md","---\nname: 部署偏好\ndescription: 先解释变更影响，再执行部署\ntype: user\n---\n先解释变更影响，再执行部署。\n").unwrap();
}
/// Manager routes also drive pure fixture tests; other memory scenes keep their setup.
pub fn manager_focus(
    scene: &str,
    host: &str,
) -> Option<(crate::ai::memory_model::Focus, bool, bool)> {
    use crate::ai::memory_model::Focus;
    Some(match scene {
        "memory-manager" => (Focus::current(Some(host)), false, true),
        "memory-manager-empty" => (Focus::default(), false, false),
        "memory-manager-edit" => (
            Focus {
                host: Some(host.into()),
                path: Some("/memories/host/deploy.md".into()),
            },
            false,
            true,
        ),
        "memory-manager-agents" => (Focus::global_instructions(), false, false),
        "memory-manager-new" => (Focus::default(), true, false),
        _ => return None,
    })
}
pub fn conversation() -> shellmate_ai::Conversation {
    use shellmate_ai::*;
    let mut c = Conversation::default();
    c.push_user(UserTurn::new(
        "记住：部署前先告诉我变更的影响范围。",
        ContextSnapshot::default(),
    ));
    c.append(Entry::Step(Step {message_id:1,call:LoggedCall {id:"memory-fixture".into(),name:"memory".into(),args_json:serde_json::json!({"command":"create","path":"/memories/global/deploy.md","file_text":"部署前先解释影响范围"})},result:LoggedResult {attachments: Vec::new(), subagent: None,inline:"已保存记忆 · global/deploy.md".into(),output_id:None,cleared:false,is_error:false},display:None}));
    c.push_text("已记住。以后部署前，我会先说明影响范围。");
    c
}
#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_store::HostStore;
    #[test]
    fn scenes_registered_in_both_themes_and_manager_has_three_topics() {
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
        }
        let root = tempfile::tempdir().unwrap();
        let mut db = HostStore::open_in_memory().unwrap();
        let host = shellmate_core::Host::new("prod-web-01", "127.0.0.1");
        db.save_host(&host).unwrap();
        seed(root.path(), host.id.as_str());
        let model = crate::ai::memory_model::Model::new(
            root.path().to_path_buf(),
            &db,
            Some(host.id.as_str()),
            &crate::ai::memory_model::Focus::current(Some(host.id.as_str())),
        )
        .unwrap();
        assert_eq!(model.files.len(), 5);
        assert!(
            model
                .read("/memories/host/AGENTS.md")
                .unwrap()
                .contains("影响范围")
        );
        assert_eq!(
            conversation().memory_action(0, 0).unwrap().path,
            "/memories/global/deploy.md"
        );
    }
    #[test]
    fn manager_scenes_cover_list_empty_structured_agents_and_creation() {
        use crate::ai::{
            memory_edit::{Content, Page},
            memory_model::Model,
        };
        for scene in [
            "memory-manager",
            "memory-manager-empty",
            "memory-manager-edit",
            "memory-manager-agents",
            "memory-manager-new",
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut db = HostStore::open_in_memory().unwrap();
            let host = shellmate_core::Host::new("prod-web-01", "127.0.0.1");
            db.save_host(&host).unwrap();
            let (focus, create, populated) = manager_focus(scene, host.id.as_str()).unwrap();
            if populated {
                seed(root.path(), host.id.as_str());
            }
            let model =
                Model::new(root.path().into(), &db, Some(host.id.as_str()), &focus).unwrap();
            let mut page = Page::from_focus(&model, focus.path.as_deref()).unwrap();
            if create {
                page.create();
            }
            assert!(!page.dirty());
            match scene {
                "memory-manager" => {
                    assert!(page.detail.is_none());
                    assert_eq!(model.topics().count(), 3);
                    assert!(model.index().is_some() && model.instructions_exist());
                }
                "memory-manager-empty" => {
                    assert!(page.detail.is_none());
                    assert!(model.memories_empty());
                    assert!(!model.instructions_exist());
                    assert_eq!(model.selected, 0);
                }
                "memory-manager-edit" => {
                    let detail = page.detail.as_ref().unwrap();
                    assert!(
                        matches!(&detail.draft.content, Content::Topic(t) if t.kind == 2 && t.name == "部署约定")
                    );
                    assert!(detail.exists);
                }
                "memory-manager-agents" => {
                    let detail = page.detail.as_ref().unwrap();
                    assert!(
                        matches!(&detail.draft.content, Content::Instructions(text) if text.is_empty())
                    );
                    assert!(!detail.exists);
                }
                "memory-manager-new" => {
                    let detail = page.detail.as_ref().unwrap();
                    assert!(detail.path.is_none() && !detail.exists);
                    assert!(matches!(&detail.draft.content, Content::Topic(t) if t.kind == 2));
                }
                _ => unreachable!(),
            }
            assert_eq!(
                super::super::request(&format!("{scene}@1100x700")),
                (scene, Some((1100, 700)))
            );
        }
    }
}
