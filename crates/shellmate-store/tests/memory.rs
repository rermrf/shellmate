use shellmate_core::{Host, HostId, memory::Memory};
use shellmate_store::HostStore;
#[test]
fn host_deletion_removes_only_its_memory_and_rejected_deletion_keeps_it() {
    let root = tempfile::tempdir().unwrap();
    let mut store = HostStore::open_in_memory().unwrap();
    let host = Host::new("fixture", "127.0.0.1");
    store.save_host(&host).unwrap();
    let memory = Memory::new(root.path(), Some(host.id.as_str()));
    memory
        .create("/memories/host/nested/topic.md", "host")
        .unwrap();
    memory
        .create("/memories/global/topic.md", "global")
        .unwrap();
    assert!(
        store
            .delete_host_with_memory(&HostId::from_raw("missing"), root.path())
            .is_err()
    );
    assert!(memory.read("/memories/host/nested/topic.md").is_ok());
    store
        .delete_host_with_memory(&host.id, root.path())
        .unwrap();
    assert!(store.host(&host.id).unwrap().is_none());
    assert!(
        !root
            .path()
            .join("memory/hosts")
            .join(host.id.as_str())
            .exists()
    );
    assert_eq!(memory.read("/memories/global/topic.md").unwrap(), "global");
}

#[test]
fn cleanup_failure_is_a_warning_after_the_host_record_is_deleted() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut store = HostStore::open_in_memory().unwrap();
    let host = Host::new("fixture", "127.0.0.1");
    store.save_host(&host).unwrap();
    std::fs::create_dir_all(root.path().join("memory/hosts")).unwrap();
    std::fs::write(outside.path().join("keep.md"), "untouched").unwrap();
    std::os::unix::fs::symlink(
        outside.path(),
        root.path().join("memory/hosts").join(host.id.as_str()),
    )
    .unwrap();
    let warning = store
        .delete_host_with_memory(&host.id, root.path())
        .unwrap()
        .unwrap();
    assert!(warning.contains("主机记忆未能清除"));
    assert!(store.host(&host.id).unwrap().is_none());
    assert_eq!(
        std::fs::read_to_string(outside.path().join("keep.md")).unwrap(),
        "untouched"
    );
}
