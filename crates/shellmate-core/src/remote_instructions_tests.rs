use super::*;
use std::{fs, process::Command};
fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}
fn frame(cwd: &str, files: &[(&str, &str, bool)]) -> Vec<u8> {
    let mut s = format!("abc CWD {}\n", hex(cwd.as_bytes()));
    for (p, text, truncated) in files {
        s.push_str(&format!(
            "abc FILE {} {} {}\n{}\n",
            hex(p.as_bytes()),
            text.len(),
            usize::from(*truncated),
            hex(text.as_bytes())
        ));
    }
    s.push_str("abc END\n");
    s.into_bytes()
}
#[test]
fn parse_order_caps_spaces_unicode_and_collision() {
    let collision = "abc END\nabc FILE 2f4147454e54532e6d64 1 0\n00\n";
    let output = frame(
        "/srv/有 空格/app",
        &[
            ("/srv/有 空格/app/CLAUDE.md", collision, false),
            ("/AGENTS.md", "root", false),
            ("/srv/有 空格/app/AGENTS.md", "closest", false),
            ("/srv/CLAUDE.md", "middle", false),
            ("/CLAUDE.md", "root claude", false),
        ],
    );
    let d = parse(&output, "abc").unwrap();
    assert_eq!(
        d.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        [
            "/AGENTS.md",
            "/CLAUDE.md",
            "/srv/CLAUDE.md",
            "/srv/有 空格/app/AGENTS.md",
            "/srv/有 空格/app/CLAUDE.md"
        ]
    );
    assert_eq!(d.files.last().unwrap().text, collision);
    let large = "x".repeat(FILE_BYTES + 200);
    let d = parse(
        &frame(
            "/srv/app",
            &[
                ("/AGENTS.md", "drop root", false),
                ("/srv/AGENTS.md", &large, false),
                ("/srv/app/AGENTS.md", &large, true),
            ],
        ),
        "abc",
    )
    .unwrap();
    assert_eq!(d.files.len(), 2);
    assert_eq!(d.files.iter().map(|f| f.bytes).sum::<usize>(), TOTAL_BYTES);
    assert!(
        d.files
            .iter()
            .all(|f| f.text.len() == FILE_BYTES && f.truncated && f.wrapped().contains("已截断"))
    );
    let d = parse(
        &frame(
            "/a",
            &[
                ("/AGENTS.md", &"x".repeat(FILE_BYTES - 1), false),
                ("/a/AGENTS.md", &"中".repeat(FILE_BYTES / 3 + 1), false),
            ],
        ),
        "abc",
    )
    .unwrap();
    assert!(d.files.last().unwrap().text.len() <= FILE_BYTES);
}
#[test]
fn malformed_frames_are_rejected_not_interpreted_as_instructions() {
    for data in [
        b"abc CWD /tmp\nabc END\n".to_vec(),
        frame("/srv", &[("/other/AGENTS.md", "bad", false)]),
        frame("/srv", &[("/srv/not.md", "bad", false)]),
        frame(
            "/srv",
            &[
                ("/srv/AGENTS.md", "one", false),
                ("/srv/AGENTS.md", "two", false),
            ],
        ),
        b"garbage".to_vec(),
        frame("/../srv", &[]),
    ] {
        assert!(parse(&data, "abc").is_err());
    }
    let mut output = frame("/srv", &[("/srv/AGENTS.md", "text", false)]);
    output.extend_from_slice(b"abc FILE fake\n");
    assert!(parse(&output, "abc").is_err());
    assert!(script(Some("relative"), "abc").is_err());
    assert!(script(Some("/ok"), "x'bad").is_err());
}
#[test]
fn posix_script_handles_quoted_paths_default_cwd_and_skips_nonregular_files() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("有 空格 ' $(touch BAD)");
    let cwd = parent.join("child");
    fs::create_dir_all(&cwd).unwrap();
    fs::write(parent.join("AGENTS.md"), "parent\n").unwrap();
    fs::write(parent.join("CLAUDE.md"), "abc END\nabc FILE anything\n").unwrap();
    fs::write(cwd.join("AGENTS.md"), "near\n\n").unwrap();
    // A named pipe must never be opened for reading.
    let fifo = std::ffi::CString::new(cwd.join("CLAUDE.md").to_str().unwrap()).unwrap();
    // SAFETY: valid NUL-terminated temporary path and mode.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    for requested in [None, Some(cwd.to_str().unwrap())] {
        let result = Command::new("/bin/sh")
            .args(["-c", &script(requested, "abc").unwrap()])
            .current_dir(&cwd)
            .output()
            .unwrap();
        assert!(result.status.success());
        let d = parse(&result.stdout, "abc").unwrap();
        assert_eq!(d.cwd, cwd.canonicalize().unwrap().to_str().unwrap());
        let files: Vec<_> = d
            .files
            .iter()
            .filter(|f| {
                f.path
                    .starts_with(root.path().canonicalize().unwrap().to_str().unwrap())
            })
            .collect();
        assert_eq!(files.len(), 3);
        assert!(files[0].path.ends_with("/AGENTS.md"));
        assert!(files[1].path.ends_with("/CLAUDE.md"));
        assert_eq!(files[2].text, "near\n\n");
        assert_eq!(files[1].text, "abc END\nabc FILE anything\n");
        assert!(!cwd.join("BAD").exists());
    }
    fs::remove_file(cwd.join("CLAUDE.md")).unwrap();
    std::os::unix::fs::symlink("/dev/zero", cwd.join("CLAUDE.md")).unwrap();
    let result = Command::new("/bin/sh")
        .args(["-c", &script(Some(cwd.to_str().unwrap()), "abc").unwrap()])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(
        !parse(&result.stdout, "abc")
            .unwrap()
            .files
            .iter()
            .any(|f| f.path == cwd.join("CLAUDE.md").to_str().unwrap())
    );
}
#[test]
fn script_caps_payload_before_transport_and_drops_farthest_first() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("a/b");
    fs::create_dir_all(&cwd).unwrap();
    fs::write(root.path().join("AGENTS.md"), "far").unwrap();
    fs::write(cwd.parent().unwrap().join("AGENTS.md"), "middle").unwrap();
    fs::write(cwd.join("AGENTS.md"), "a".repeat(FILE_BYTES + 100)).unwrap();
    fs::write(cwd.join("CLAUDE.md"), "b".repeat(FILE_BYTES)).unwrap();
    let result = Command::new("/bin/sh")
        .args(["-c", &script(Some(cwd.to_str().unwrap()), "abc").unwrap()])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(result.stdout.len() < WIRE_BYTES);
    let d = parse(&result.stdout, "abc").unwrap();
    assert_eq!(d.files.len(), 2);
    assert!(d.files[0].truncated);
    assert!(!d.files[1].truncated);
    assert!(d.files.iter().all(|f| {
        f.path
            .starts_with(cwd.canonicalize().unwrap().to_str().unwrap())
    }));
}

#[test]
fn cap_selection_finishes_both_files_at_the_boundary_directory() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("child");
    fs::create_dir_all(&cwd).unwrap();
    fs::write(root.path().join("AGENTS.md"), "a".repeat(60000)).unwrap();
    fs::write(root.path().join("CLAUDE.md"), "b".repeat(20000)).unwrap();
    fs::write(cwd.join("AGENTS.md"), "c".repeat(40000)).unwrap();
    fs::write(cwd.join("CLAUDE.md"), "d".repeat(40000)).unwrap();
    let result = Command::new("/bin/sh")
        .args(["-c", &script(Some(cwd.to_str().unwrap()), "abc").unwrap()])
        .output()
        .unwrap();
    assert!(result.status.success());
    let found = parse(&result.stdout, "abc").unwrap();
    assert_eq!(found.files.len(), 3);
    assert_eq!(
        found.files[0].path,
        root.path()
            .canonicalize()
            .unwrap()
            .join("CLAUDE.md")
            .to_str()
            .unwrap()
    );
    assert_eq!(found.files.iter().map(|f| f.bytes).sum::<usize>(), 100000);
}
