use super::*;
use crate::ai::memory_model::Focus;
use shellmate_core::{
    Host,
    memory::{FILE_BYTES, Memory},
};
use shellmate_store::HostStore;
fn fixture(focus: Focus) -> (tempfile::TempDir, Model, Page) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = HostStore::open_in_memory().unwrap();
    let host = Host::new("prod", "127.0.0.1");
    db.save_host(&host).unwrap();
    let model = Model::new(
        dir.path().to_path_buf(),
        &db,
        Some(host.id.as_str()),
        &focus,
    )
    .unwrap();
    let page = Page::from_focus(&model, focus.path.as_deref()).unwrap();
    (dir, model, page)
}
fn topic(page: &mut Page) -> &mut Topic {
    match &mut page.detail.as_mut().unwrap().draft.content {
        Content::Topic(topic) => topic,
        _ => panic!("structured topic"),
    }
}
#[test]
fn frontmatter_roundtrip_preserves_fields_body_and_safe_special_characters() {
    let mut value = Topic {
        name: "发布: '蓝绿' #部署 \"v2\"".into(),
        description: "  leading and trailing  ".into(),
        kind: 2,
        body: "# Body\n\ncode: value\n---\n中文\n".into(),
    };
    let text = value.serialize();
    assert_eq!(Topic::parse(&text), Some(value.clone()));
    assert!(text.contains("name: '发布: ''蓝绿'' #部署 \"v2\"'"));
    assert!(text.contains("description: '  leading and trailing  '"));
    value.name = "multi\nline\r\nname".into();
    value.description = "true".into();
    let parsed = Topic::parse(&value.serialize()).unwrap();
    assert_eq!(parsed.name, "multilinename");
    assert_eq!(parsed.description, "true");
    value.name = "tab\tand\0control".into();
    assert_eq!(Topic::parse(&value.serialize()), Some(value));
    let text = "---\r\nname: 'server''s note'\r\ndescription: \"say \\\"hello\\\"\"\r\ntype: reference\r\nmodified: 2026-11-25T00:00:00+08:00\r\n---\r\n\r\nfirst\r\n\r\n";
    let parsed = Topic::parse(text).unwrap();
    assert_eq!(parsed.name, "server's note");
    assert_eq!(parsed.description, "say \"hello\"");
    assert_eq!(parsed.kind, 3);
    assert_eq!(parsed.body, "first\r\n\r\n");
    assert_eq!(Topic::parse(&parsed.serialize()), Some(parsed));
}
#[test]
fn unknown_malformed_or_unrepresentable_frontmatter_falls_back_without_loss() {
    for text in [
        "---\nname:bad\ndescription: y\ntype: project\n---\nbody",
        "plain markdown\n",
        "---\nname: unclosed\n",
        "---\nname: x\ntype: project\n---\nbody",
        "---\nname: x\ndescription: y\ntype: unknown\n---\nbody",
        "---\nname: x\ndescription: y\ntype: project\nextra: keep me\n---\nbody",
        "---\nname: x\nname: y\ndescription: y\ntype: project\n---\nbody",
        "---\nname: 'broken\ndescription: y\ntype: project\n---\nbody",
        "---\nname: x\ndescription: |\n  multiline\ntype: project\n---\nbody",
        "---\nname: x\ndescription: [a, b]\ntype: project\n---\nbody",
        "---\nname: x\ndescription: y # preserve comment\ntype: project\n---\nbody",
        "---\nname: x\ndescription: \"multi\\nline\"\ntype: project\n---\nbody",
    ] {
        let detail = Detail::load("/memories/global/topic.md".into(), text.into(), true);
        assert!(matches!(detail.draft.content, Content::Raw(_)), "{text}");
        assert_eq!(detail.draft.content.text(), text);
        assert!(!detail.dirty());
    }
}
#[test]
fn raw_editor_saves_exact_bytes_even_with_unknown_fields_and_modified() {
    let (_dir, mut model, mut page) = fixture(Focus::default());
    for (n, text) in [
        "plain markdown\n",
        "---\nname: broken\n",
        "---\r\nname: old\r\ncustom: [keep, this]\r\nmodified: do-not-rewrite\r\n---\r\nbody\r\n",
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("/memories/global/raw{n}.md");
        model.memory().save_verbatim(&path, text).unwrap();
        page.open(&model, &path).unwrap();
        assert!(matches!(
            page.detail.as_ref().unwrap().draft.content,
            Content::Raw(_)
        ));
        let edited = format!("{text}additional body\n");
        *page.detail.as_mut().unwrap().draft.content.body_mut() = edited.clone();
        assert!(page.dirty());
        page.save(&mut model).unwrap();
        assert!(!page.dirty());
        assert_eq!(model.memory().read(&path).unwrap(), edited);
        *page.detail.as_mut().unwrap().draft.content.body_mut() = "x".repeat(FILE_BYTES + 1);
        assert!(page.save(&mut model).is_err());
        assert!(page.dirty());
        assert_eq!(model.memory().read(&path).unwrap(), edited);
    }
}
#[test]
fn dirty_tracking_covers_every_field_and_save_resets_it() {
    let (_dir, mut model, mut page) = fixture(Focus::default());
    page.create();
    assert!(!page.dirty());
    assert_eq!(topic(&mut page).kind, 2);
    page.detail.as_mut().unwrap().draft.filename = "deploy.md".into();
    assert!(page.dirty());
    page.save(&mut model).unwrap();
    assert!(!page.dirty());
    assert!(page.detail.as_ref().unwrap().exists);
    for field in 0..4 {
        let t = topic(&mut page);
        match field {
            0 => t.name = "Deployment".into(),
            1 => t.description = "Durable convention".into(),
            2 => t.kind = 1,
            _ => t.body = "body changed".into(),
        }
        assert!(page.dirty());
        page.save(&mut model).unwrap();
        assert!(!page.dirty());
    }
    assert!(
        model
            .memory()
            .read("/memories/global/deploy.md")
            .unwrap()
            .contains("modified:")
    );
    page.open(&model, "/memories/global/AGENTS.md").unwrap();
    assert!(!page.dirty());
    assert!(!page.detail.as_ref().unwrap().exists);
    *page.detail.as_mut().unwrap().draft.content.body_mut() = "请用中文".into();
    assert!(page.dirty());
    page.save(&mut model).unwrap();
    assert!(!page.dirty());
    assert!(page.detail.as_ref().unwrap().exists);
}
#[test]
fn navigation_focus_back_scope_close_confirm_cancel_and_delete() {
    let (_dir, mut model, mut page) = fixture(Focus::global_instructions());
    assert!(matches!(
        page.detail.as_ref().unwrap().draft.content,
        Content::Instructions(_)
    ));
    assert_eq!(page.subtitle(&model), "全局 · AGENTS.md");
    assert_eq!(
        page.navigate(&mut model, Navigation::Back, false).unwrap(),
        NavigationResult::Applied
    );
    assert!(page.detail.is_none());
    assert_eq!(model.selected, 0);
    page.create();
    page.detail.as_mut().unwrap().draft.filename = "note.md".into();
    for nav in [Navigation::Back, Navigation::Scope(1), Navigation::Close] {
        assert_eq!(
            page.navigate(&mut model, nav, false).unwrap(),
            NavigationResult::NeedsConfirmation
        );
        assert_eq!(model.selected, 0);
        assert!(page.dirty());
    }
    assert_eq!(
        page.navigate(&mut model, Navigation::Scope(0), false)
            .unwrap(),
        NavigationResult::Applied
    );
    assert!(page.dirty());
    assert_eq!(
        page.navigate(&mut model, Navigation::Scope(1), true)
            .unwrap(),
        NavigationResult::Applied
    );
    assert_eq!(model.selected, 1);
    assert!(page.detail.is_none());
    page.create();
    page.detail.as_mut().unwrap().draft.filename = "local.md".into();
    page.save(&mut model).unwrap();
    assert_eq!(page.subtitle(&model), "本机终端 · local.md");
    page.delete(&mut model).unwrap();
    assert!(page.detail.is_none());
    assert!(model.memories_empty());
    page.create();
    assert_eq!(
        page.navigate(&mut model, Navigation::Close, false).unwrap(),
        NavigationResult::Close
    );
    assert!(Page::from_focus(&model, None).unwrap().detail.is_none());
}
#[test]
fn list_model_omits_missing_index_and_distinguishes_missing_instructions() {
    let (_dir, mut model, _page) = fixture(Focus::default());
    assert!(model.memories_empty());
    assert!(!model.instructions_exist());
    assert!(model.index().is_none());
    model.save("/memories/global/AGENTS.md", "").unwrap();
    model.reload().unwrap();
    assert!(model.instructions_exist());
    assert!(model.memories_empty());
    model.create("z.md", &Topic::default().serialize()).unwrap();
    model.reload().unwrap();
    assert!(!model.memories_empty());
    assert!(model.index().is_none());
    assert_eq!(model.topics().count(), 1);
    model.save("/memories/global/MEMORY.md", "index").unwrap();
    model.reload().unwrap();
    assert!(model.index().is_some());
    assert_eq!(model.topics().count(), 1);
}
#[test]
fn creation_validation_keeps_dirty_draft_until_success() {
    let (_dir, mut model, mut page) = fixture(Focus::default());
    page.create();
    for invalid in ["../bad.md", "wrong.txt", "AGENTS.md"] {
        page.detail.as_mut().unwrap().draft.filename = invalid.into();
        assert!(page.save(&mut model).is_err());
        assert!(page.dirty());
        assert!(page.detail.as_ref().unwrap().path.is_none());
    }
    model.create("exists.md", "existing").unwrap();
    page.detail.as_mut().unwrap().draft.filename = "exists.md".into();
    assert!(page.save(&mut model).is_err());
    page.detail.as_mut().unwrap().draft.filename = "valid.md".into();
    topic(&mut page).body = "x".repeat(FILE_BYTES);
    assert!(page.save(&mut model).is_err());
    assert!(page.dirty());
    topic(&mut page).body = "saved".into();
    page.save(&mut model).unwrap();
    assert!(!page.dirty());
    assert_eq!(
        page.detail.as_ref().unwrap().path.as_deref(),
        Some("/memories/global/valid.md")
    );
    assert_eq!(
        Memory::new(&model.root, None)
            .read("/memories/global/exists.md")
            .unwrap(),
        "existing"
    );
}

#[test]
fn subtitle_uses_scope_label_and_filename_without_internal_ids() {
    let (_dir, mut model, mut page) = fixture(Focus::global_instructions());
    assert_eq!(page.subtitle(&model), "全局 · AGENTS.md");
    page.navigate(&mut model, Navigation::Scope(1), false)
        .unwrap();
    page.open(&model, "/memories/host/MEMORY.md").unwrap();
    assert_eq!(page.subtitle(&model), "本机终端 · MEMORY.md");
    page.navigate(&mut model, Navigation::Scope(2), false)
        .unwrap();
    model.scopes[2].label = "prod-web-01".into();
    page.open(&model, "/memories/host/subdir/deploy.md")
        .unwrap();
    assert_eq!(page.subtitle(&model), "prod-web-01 · deploy.md");
    assert!(
        !page
            .subtitle(&model)
            .contains(model.scopes[2].key.as_deref().unwrap())
    );
    page.navigate(&mut model, Navigation::Scope(0), false)
        .unwrap();
    page.create();
    assert_eq!(page.subtitle(&model), "全局");
    page.detail.as_mut().unwrap().draft.filename = "draft.md".into();
    assert_eq!(page.subtitle(&model), "全局");
}
