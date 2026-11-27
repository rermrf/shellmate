use super::*;
use shellmate_ai::{
    AnswerItem, ContextSnapshot, Conversation, Entry, LoggedCall, LoggedResult, Step, UserTurn,
};
use shellmate_core::Host;
fn setup() -> (tempfile::TempDir, HostStore, Host, Host, Host) {
    let root = tempfile::tempdir().unwrap();
    let mut db = HostStore::open_in_memory().unwrap();
    let current = Host::new("current", "127.0.0.1");
    let stored = Host::new("stored", "127.0.0.2");
    let other = Host::new("other", "127.0.0.3");
    for h in [&current, &stored, &other] {
        db.save_host(h).unwrap();
    }
    Memory::new(root.path(), Some(stored.id.as_str()))
        .create("/memories/host/topic.md", TEMPLATE)
        .unwrap();
    (root, db, current, stored, other)
}
#[test]
fn scopes_include_global_local_current_and_existing_memory_only() {
    let (root, db, current, stored, other) = setup();
    let model = Model::new(
        root.path().into(),
        &db,
        Some(current.id.as_str()),
        &Focus::current(Some(current.id.as_str())),
    )
    .unwrap();
    assert_eq!(model.scopes.len(), 4);
    assert_eq!(model.scopes[0].key, None);
    assert_eq!(model.scopes[1].key.as_deref(), Some("local"));
    assert_eq!(
        model.scopes[model.selected].key.as_deref(),
        Some(current.id.as_str())
    );
    assert!(
        model
            .scopes
            .iter()
            .any(|s| s.key.as_deref() == Some(stored.id.as_str()))
    );
    assert!(
        !model
            .scopes
            .iter()
            .any(|s| s.key.as_deref() == Some(other.id.as_str()))
    );
    assert!(model.files.is_empty());
}
#[test]
fn manager_create_save_delete_stamp_limits() {
    let (root, db, current, _, _) = setup();
    let mut model = Model::new(
        root.path().into(),
        &db,
        Some(current.id.as_str()),
        &Focus::current(Some(current.id.as_str())),
    )
    .unwrap();
    model
        .save("/memories/host/AGENTS.md", "instructions")
        .unwrap();
    assert_eq!(
        model.read("/memories/host/AGENTS.md").unwrap(),
        "instructions"
    );
    let path = model.create("deploy.md", TEMPLATE).unwrap();
    assert!(model.read(&path).unwrap().contains("modified:"));
    model.save(&path, &format!("{TEMPLATE}body")).unwrap();
    assert!(model.read(&path).unwrap().ends_with("body"));
    for bad in [
        "../escape.md",
        "/outside.md",
        "topic.txt",
        "AGENTS.md",
        "MEMORY.md",
        "a\0.md",
    ] {
        assert!(model.create(bad, TEMPLATE).is_err());
    }
    assert!(model.save(&path, &"x".repeat(FILE_BYTES + 1)).is_err());
    assert!(model.read(&path).unwrap().ends_with("body"));
    assert!(model.create("deploy.md", TEMPLATE).is_err());
    for n in 2..200 {
        model.create(&format!("{n}.md"), "x").unwrap();
    }
    assert!(model.create("overflow.md", "x").is_err());
    model.save(&path, "edit at capacity").unwrap();
    model.delete(&path).unwrap();
    model.create("replacement.md", "x").unwrap();
    assert!(model.delete(&path).is_err());
    model.reload().unwrap();
    assert_eq!(model.files.len(), 200);
}
#[test]
fn clear_all_removes_only_memory_and_leaves_conversations() {
    let (root, mut db, current, _, _) = setup();
    Memory::new(root.path(), Some("local"))
        .save("/memories/host/AGENTS.md", "local")
        .unwrap();
    Memory::new(root.path(), None)
        .save("/memories/global/AGENTS.md", "global")
        .unwrap();
    db.set_auto_memory(false).unwrap();
    db.set_setting("other", "keep").unwrap();
    let mut conversation = Conversation::default();
    conversation.push_user(UserTurn::new("preserve", ContextSnapshot::default()));
    db.save_ai_conversation(&shellmate_store::ai::SavedConversation {
        id: "test-memory-clear".into(),
        host_id: Some(current.id.to_string()),
        label: "host".into(),
        title: "preserve".into(),
        provider: "fixture".into(),
        model: "fixture".into(),
        created_at: 1,
        updated_at: 1,
        conversation,
    })
    .unwrap();
    std::fs::create_dir(root.path().join("ai-outputs")).unwrap();
    std::fs::write(root.path().join("ai-outputs/keep"), "keep").unwrap();
    clear_all(root.path()).unwrap();
    assert!(!root.path().join("memory").exists());
    assert!(root.path().join("ai-outputs/keep").exists());
    assert!(!db.ai_settings().unwrap().auto_memory);
    assert_eq!(db.setting("other").unwrap().as_deref(), Some("keep"));
    assert!(db.ai_conversation("test-memory-clear").unwrap().is_some());
    assert_eq!(db.hosts().unwrap().len(), 3);
}
#[test]
fn host_note_requires_saved_id_then_saves_edits_and_empty_deletes() {
    let root = tempfile::tempdir().unwrap();
    let mut db = HostStore::open_in_memory().unwrap();
    let host = Host::new("new", "127.0.0.1");
    assert!(save_host_note(root.path(), &db, &host.id, "note").is_err());
    assert!(!root.path().join("memory").exists());
    db.save_host(&host).unwrap();
    save_host_note(root.path(), &db, &host.id, "note").unwrap();
    let memory = Memory::new(root.path(), Some(host.id.as_str()));
    assert_eq!(memory.read("/memories/host/AGENTS.md").unwrap(), "note");
    save_host_note(root.path(), &db, &host.id, "updated").unwrap();
    assert_eq!(memory.read("/memories/host/AGENTS.md").unwrap(), "updated");
    assert!(save_host_note(root.path(), &db, &host.id, &"x".repeat(FILE_BYTES + 1)).is_err());
    save_host_note(root.path(), &db, &host.id, " \n").unwrap();
    assert!(memory.read("/memories/host/AGENTS.md").is_err());
    save_host_note(root.path(), &db, &host.id, "").unwrap();
}
#[test]
fn memory_command_is_local_while_busy_and_focus_uses_scope_not_notice_text() {
    use crate::ai::commands::{COMMANDS, Dispatch, dispatch};
    assert!(matches!(
        dispatch("/memory", true, COMMANDS),
        Dispatch::Run { name: "memory", .. }
    ));
    assert_eq!(Focus::current(Some("local")).host.as_deref(), Some("local"));
    assert_eq!(Focus::current(None), Focus::default());
    for (command, path, expected, scope_only) in [
        ("create", "/memories/global/deploy.md", None, false),
        ("str_replace", "/memories/host/a.md", Some("saved"), false),
        ("insert", "/memories/host/a.md", Some("saved"), false),
        ("rename", "/memories/host/new.md", Some("saved"), false),
        ("delete", "/memories/host/a.md", Some("saved"), true),
    ] {
        let args = if command == "rename" {
            serde_json::json!({"command":command,"old_path":"/memories/global/old.md","new_path":path})
        } else {
            serde_json::json!({"command":command,"path":path})
        };
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("question", ContextSnapshot::default()));
        log.push_text("prefix");
        log.append(Entry::Step(Step {
            message_id: 2,
            call: LoggedCall {
                id: "call".into(),
                name: "memory".into(),
                args_json: args,
            },
            result: LoggedResult {
                attachments: Vec::new(), subagent: None,
                inline: "unrelated result text".into(),
                output_id: None,
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
        log.push_item(AnswerItem::Notice("已保存记忆 · /misleading.md".into()));
        let restored = Conversation::from_entries(
            serde_json::from_str(&serde_json::to_string(log.entries()).unwrap()).unwrap(),
        );
        let action = restored.memory_action(0, 1).unwrap();
        assert_eq!(action.path, path);
        assert_eq!(action.scope_only, scope_only);
        assert!(restored.memory_action(0, 2).is_none());
        let focus = Focus::from_action(action, Some("saved")).unwrap();
        assert_eq!(focus.host.as_deref(), expected);
        assert_eq!(focus.path.is_none(), scope_only);
    }
    for command in ["view", "bogus"] {
        assert!(
            shellmate_ai::memory::action(
                &serde_json::json!({"command":command,"path":"/memories/global/a.md"}),
                false
            )
            .is_none()
        );
    }
    assert!(
        shellmate_ai::memory::action(
            &serde_json::json!({"command":"create","path":"/memories/global/a.md"}),
            true
        )
        .is_none()
    );
    assert!(
        Focus::from_action(
            &shellmate_ai::memory::Action {
                path: "/memories/host/a.md".into(),
                scope_only: false
            },
            None
        )
        .is_none()
    );
}
#[test]
fn global_toggle_is_shared_between_settings_and_manager() {
    let mut db = HostStore::open_in_memory().unwrap();
    assert!(db.ai_settings().unwrap().auto_memory);
    db.set_auto_memory(false).unwrap();
    let mut settings = db.ai_settings().unwrap();
    settings.model = "changed".into();
    db.save_ai_settings(&settings).unwrap();
    assert!(!db.ai_settings().unwrap().auto_memory);
    db.set_auto_memory(true).unwrap();
    assert!(db.ai_settings().unwrap().limits().auto_memory);
    db.set_setting("ai_auto_memory", "invalid").unwrap();
    assert!(db.ai_settings().unwrap().auto_memory);
}
