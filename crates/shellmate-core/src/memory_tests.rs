use super::*;
use std::os::unix::fs::symlink;
const TOPIC: &str = "---\nname: deploy\ndescription: persistent layout\ntype: project # keep this comment\n---\nbody\n";
fn fixture() -> (tempfile::TempDir, Memory) {
    let dir = tempfile::tempdir().unwrap();
    let memory = Memory::new(dir.path(), Some("fixture-host"));
    (dir, memory)
}
#[test]
fn all_commands_listing_ranges_and_unique_replacement() {
    let (_dir, m) = fixture();
    for path in ["/memories", "//memories//./"] {
        assert_eq!(m.view(path, None).unwrap(), "global/\nhost/");
    }
    assert_eq!(m.view("/memories/global", None).unwrap(), "（空目录）");
    let p = "/memories/global/sub/a.md";
    assert!(
        m.create(p, "one\ntwo\nthree")
            .unwrap()
            .contains("已保存记忆 · global/sub/a.md")
    );
    assert!(m.create(p, "overwrite").is_err());
    assert_eq!(m.view(p, Some([2, 3])).unwrap(), "2: two\n3: three");
    assert!(m.view(p, Some([0, 2])).is_err());
    assert!(m.view(p, Some([3, 2])).is_err());
    assert!(m.view(p, Some([5, 8])).is_err());
    assert!(m.view("/memories/global", Some([1, 2])).is_err());
    let listing = m.view("/memories/global/sub", None).unwrap();
    assert!(listing.contains("a.md\t13 bytes\tmodified: "));
    assert!(m.view("/memories/global", None).unwrap().contains("sub/"));
    m.str_replace(p, "two", "TWO").unwrap();
    assert!(m.str_replace(p, "missing", "").is_err());
    assert!(m.str_replace(p, "", "").is_err());
    m.insert(p, 0, "zero").unwrap();
    m.insert(p, 2, "inserted\n").unwrap();
    m.insert(p, 5, "tail").unwrap();
    assert_eq!(m.read(p).unwrap(), "zero\none\ninserted\nTWO\nthree\ntail");
    assert!(m.insert(p, 99, "bad").is_err());
    m.str_replace(p, "zero", "three").unwrap();
    assert!(m.str_replace(p, "three", "").is_err());
    m.create("/memories/global/overlap.md", "aaa").unwrap();
    assert!(
        m.str_replace("/memories/global/overlap.md", "aa", "")
            .is_err()
    );
    let new = "/memories/host/moved.md";
    assert!(
        m.rename(p, new)
            .unwrap()
            .contains("已更新记忆 · host/moved.md")
    );
    assert!(m.read(p).is_err());
    assert!(m.rename(new, new).is_err());
    assert!(m.rename(new, "/memories/global/overlap.md").is_err());
    assert!(
        m.delete(new)
            .unwrap()
            .contains("已删除记忆 · host/moved.md")
    );
    assert!(m.read(new).is_err());
}
#[test]
fn normalized_paths_and_all_escape_forms_are_rejected() {
    let (dir, m) = fixture();
    m.create("/memories//global/./sub//a.md", "safe").unwrap();
    assert_eq!(m.read("/memories/global/sub/a.md").unwrap(), "safe");
    for bad in [
        "/memories/global/../host/a.md",
        "/tmp/a.md",
        "global/a.md",
        "/memories/global/a\0.md",
        "/memories/global/a\\b.md",
        "/memories/global/a.txt",
        "/memories/global/a.MD",
        "/memories/global",
        "/memories/other/a.md",
    ] {
        assert!(m.create(bad, "bad").is_err(), "{bad}");
    }
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.md"), "untouched").unwrap();
    symlink(&outside, dir.path().join("memory/global/link")).unwrap();
    symlink(
        outside.join("secret.md"),
        dir.path().join("memory/global/link.md"),
    )
    .unwrap();
    symlink(
        dir.path().join("missing"),
        dir.path().join("memory/global/dangling.md"),
    )
    .unwrap();
    for path in [
        "/memories/global/link/secret.md",
        "/memories/global/link.md",
        "/memories/global/dangling.md",
    ] {
        assert!(m.view(path, None).is_err());
        assert!(m.create(path, "bad").is_err());
        assert!(m.str_replace(path, "untouched", "bad").is_err());
        assert!(m.insert(path, 0, "bad").is_err());
        assert!(m.delete(path).is_err());
        assert!(m.rename(path, "/memories/global/renamed.md").is_err());
        assert!(m.rename("/memories/global/sub/a.md", path).is_err());
    }
    assert_eq!(
        fs::read_to_string(outside.join("secret.md")).unwrap(),
        "untouched"
    );
    let empty = tempfile::tempdir().unwrap();
    symlink(&outside, empty.path().join("memory")).unwrap();
    assert!(
        Memory::new(empty.path(), None)
            .create("/memories/global/a.md", "bad")
            .is_err()
    );
}
#[test]
fn unsaved_sessions_and_invalid_host_keys_have_no_host_access() {
    let dir = tempfile::tempdir().unwrap();
    let m = Memory::new(dir.path(), None);
    assert_eq!(m.view("/memories/", None).unwrap(), "global/");
    assert!(
        m.view("/memories/host", None)
            .unwrap_err()
            .to_string()
            .contains("没有主机记忆范围")
    );
    assert!(m.create("/memories/host/a.md", "x").is_err());
    for key in ["..", "../outside", "/escape", "", "x\0"] {
        assert!(
            Memory::new(dir.path(), Some(key))
                .create("/memories/host/a.md", "x")
                .is_err()
        );
    }
    Memory::new(dir.path(), Some("local"))
        .create("/memories/host/a.md", "local")
        .unwrap();
    assert!(dir.path().join("memory/hosts/local/a.md").exists());
}
#[test]
fn stamping_preserves_frontmatter_and_body_for_create_replace_insert_rename() {
    let (_dir, m) = fixture();
    let p = "/memories/global/deploy.md";
    m.create(p, TOPIC).unwrap();
    let text = m.read(p).unwrap();
    assert_eq!(
        text.replace(
            text.lines().find(|l| l.starts_with("modified:")).unwrap(),
            ""
        )
        .replace("\n\n---", "\n---"),
        TOPIC
    );
    let stamp_value = text
        .lines()
        .find_map(|l| l.strip_prefix("modified: "))
        .unwrap();
    assert_eq!(stamp_value.len(), 25);
    assert!(stamp_value.as_bytes()[19] == b'+' || stamp_value.as_bytes()[19] == b'-');
    assert_eq!(&stamp_value[22..23], ":");
    for operation in 0..3 {
        // Simulate an old imported timestamp, then edit through each tool operation.
        let path = m.resolve(p, true).unwrap().path;
        fs::write(
            &path,
            TOPIC.replace(
                "type: project",
                "modified: 2000-01-01T00:00:00+00:00\ntype: project",
            ),
        )
        .unwrap();
        match operation {
            0 => {
                m.str_replace(p, "body", "changed").unwrap();
            }
            1 => {
                m.insert(p, 7, "tail").unwrap();
            }
            _ => {
                m.rename(p, "/memories/global/renamed.md").unwrap();
            }
        }
        let text = m
            .read(if operation == 2 {
                "/memories/global/renamed.md"
            } else {
                p
            })
            .unwrap();
        assert!(!text.contains("2000-01-01"));
        assert_eq!(text.matches("modified:").count(), 1);
        assert!(text.contains("name: deploy\ndescription: persistent layout\n"));
        assert!(text.contains("type: project # keep this comment"));
    }
    assert_eq!(stamp("no frontmatter", "now"), "no frontmatter");
    assert_eq!(stamp("---\nname: broken\n", "now"), "---\nname: broken\n");
    assert_eq!(
        stamp("---\r\nname: a\r\n---\r\nbody", "now"),
        "---\r\nname: a\r\nmodified: now\r\n---\r\nbody"
    );
    for name in ["AGENTS.md", "MEMORY.md"] {
        let p = format!("/memories/host/{name}");
        m.create(&p, TOPIC).unwrap();
        assert_eq!(m.read(&p).unwrap(), TOPIC);
    }
}
#[test]
fn byte_and_scope_limits_are_enforced_without_partial_edits() {
    let (_dir, m) = fixture();
    let p = "/memories/global/limit.md";
    m.create(p, &"a".repeat(FILE_BYTES)).unwrap();
    assert!(m.insert(p, 0, "overflow").is_err());
    assert!(
        m.str_replace(p, &"a".repeat(FILE_BYTES), &"b".repeat(FILE_BYTES + 1))
            .is_err()
    );
    assert_eq!(m.read(p).unwrap().len(), FILE_BYTES);
    assert!(
        m.create(
            "/memories/global/large.md",
            &"界".repeat(FILE_BYTES / 3 + 1)
        )
        .is_err()
    );
    assert!(
        m.create(
            "/memories/global/stamp.md",
            &format!("{TOPIC}{}", "x".repeat(FILE_BYTES - TOPIC.len()))
        )
        .is_err()
    );
    // Manually edited UTF-8 files must not evade the read cap via a partial final character.
    let external = m.resolve("/memories/host/external.md", true).unwrap().path;
    fs::create_dir_all(external.parent().unwrap()).unwrap();
    fs::write(&external, "界".repeat(FILE_BYTES / 3 + 1)).unwrap();
    assert!(m.read("/memories/host/external.md").is_err());
    for n in 1..SCOPE_FILES {
        m.create(&format!("/memories/global/nested/{n}.md"), "x")
            .unwrap();
    }
    assert!(
        m.create("/memories/global/full.md", "x")
            .unwrap_err()
            .to_string()
            .contains("200")
    );
    m.rename(p, "/memories/global/renamed.md").unwrap();
    m.str_replace("/memories/global/nested/1.md", "x", "edit at capacity")
        .unwrap();
    m.create("/memories/host/free.md", "host").unwrap();
    assert!(
        m.rename("/memories/host/free.md", "/memories/global/full.md")
            .is_err()
    );
    assert_eq!(m.read("/memories/host/free.md").unwrap(), "host");
    m.delete("/memories/global/nested/2.md").unwrap();
    m.rename("/memories/host/free.md", "/memories/global/now-free.md")
        .unwrap();
}
#[test]
fn index_warnings_cover_both_thresholds_and_write_commands() {
    for (text, expected) in [
        ("x\n".repeat(179), ""),
        ("x\n".repeat(180), "接近"),
        ("x\n".repeat(200), "接近"),
        ("x\n".repeat(201), "已超过"),
        ("x".repeat(INDEX_BYTES * 9 / 10 - 1), ""),
        ("x".repeat(INDEX_BYTES * 9 / 10), "接近"),
        ("x".repeat(INDEX_BYTES), "接近"),
        ("x".repeat(INDEX_BYTES + 1), "已超过"),
    ] {
        let (_dir, m) = fixture();
        let p = "/memories/global/MEMORY.md";
        let result = m.create(p, &text).unwrap();
        assert_eq!(m.read(p).unwrap(), text);
        if expected.is_empty() {
            assert!(!result.contains("加载上限"));
        } else {
            assert!(result.contains(expected), "{result}");
        }
    }
    let (_dir, m) = fixture();
    let p = "/memories/global/MEMORY.md";
    m.create(p, "start").unwrap();
    assert!(
        m.str_replace(p, "start", &"x\n".repeat(180))
            .unwrap()
            .contains("接近")
    );
    assert!(
        m.insert(p, 0, &"y\n".repeat(21))
            .unwrap()
            .contains("已超过")
    );
    assert!(
        m.rename(p, "/memories/host/MEMORY.md")
            .unwrap()
            .contains("已超过")
    );
}
#[test]
fn deletion_is_host_specific_and_output_sweep_does_not_touch_memory() {
    let (dir, m) = fixture();
    m.create("/memories/global/a.md", "global").unwrap();
    m.create("/memories/host/nested/a.md", "host").unwrap();
    let local = Memory::new(dir.path(), Some("local"));
    local.create("/memories/host/a.md", "local").unwrap();
    crate::ai_output::sweep(
        dir.path(),
        SystemTime::now() + std::time::Duration::from_secs(100 * 86400),
    )
    .unwrap();
    assert!(m.read("/memories/host/nested/a.md").is_ok());
    m.delete_host().unwrap();
    assert!(!dir.path().join("memory/hosts/fixture-host").exists());
    assert!(m.read("/memories/global/a.md").is_ok());
    assert!(local.read("/memories/host/a.md").is_ok());
    m.delete_host().unwrap();
}

#[test]
fn directory_handles_never_follow_a_replacement_symlink() {
    let (dir, m) = fixture();
    m.create("/memories/global/nested/a.md", "safe").unwrap();
    let parent_path = dir
        .path()
        .canonicalize()
        .unwrap()
        .join("memory/global/nested");
    let parent = disk::Dir::open(dir.path(), &parent_path, false).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("a.md"), "outside").unwrap();
    fs::rename(&parent_path, parent_path.with_file_name("moved")).unwrap();
    symlink(outside.path(), &parent_path).unwrap();
    let mut content = String::new();
    parent
        .read(std::ffi::OsStr::new("a.md"))
        .unwrap()
        .read_to_string(&mut content)
        .unwrap();
    assert_eq!(content, "safe");
    assert!(m.read("/memories/global/nested/a.md").is_err());
    assert!(m.create("/memories/global/nested/new.md", "bad").is_err());
    parent.remove(std::ffi::OsStr::new("a.md"), false).unwrap();
    assert_eq!(
        fs::read_to_string(outside.path().join("a.md")).unwrap(),
        "outside"
    );
    assert!(!outside.path().join("new.md").exists());
}
