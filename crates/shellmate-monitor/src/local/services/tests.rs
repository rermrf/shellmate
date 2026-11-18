use super::plists::log_tail;
use super::*;
use std::fs;
use std::time::SystemTime;

const LIST: &str = include_str!("../../../tests/fixtures/macos/launchctl_list.txt");
const SYSTEM: &str = include_str!("../../../tests/fixtures/macos/launchctl_system.txt");
const DISABLED: &str = include_str!("../../../tests/fixtures/macos/launchctl_disabled.txt");
const XML: &[u8] = include_bytes!("../../../tests/fixtures/macos/agent.xml.plist");
const BINARY: &[u8] = include_bytes!("../../../tests/fixtures/macos/agent.binary.plist");

#[test]
fn parses_user_list_and_preserves_unknown_numbers() {
    let list = parse_list(LIST);
    assert!(list.complete);
    assert_eq!(list.rows["homebrew.mxcl.redis"], Some((742, Some(0))));
    assert_eq!(list.rows["org.example.crashed"], Some((0, Some(-15))));
    assert_eq!(list.rows["org.example.unknown"], None);
    assert!(!parse_list("unexpected header").complete);
}

#[test]
fn parses_only_the_complete_system_services_block() {
    let list = parse_system(SYSTEM);
    assert!(list.complete);
    assert_eq!(list.rows.len(), 4);
    assert_eq!(list.rows["org.example.agent"], Some((0, None)));
    assert_eq!(list.rows["org.example.unknown"], None);
    assert!(!parse_system("services = {\n0 - orphan").complete);
    assert!(!parse_system("unrelated = {\n}").complete);
}

#[test]
fn disabled_map_distinguishes_empty_from_unreadable() {
    let map = parse_disabled(DISABLED).unwrap();
    assert_eq!(map.get("homebrew.mxcl.redis"), Some(&false));
    assert_eq!(map.get("org.example.agent"), Some(&true));
    assert_eq!(parse_disabled("disabled services = {\n}"), Some(HashMap::new()));
    assert_eq!(parse_disabled("permission denied"), None);
}

#[tokio::test]
async fn xml_binary_and_mtime_cache_include_invalid_files_and_evict_deletions() {
    let dir = tempfile::tempdir().unwrap();
    let roots = || vec![(false, dir.path().to_owned())];
    let xml = dir.path().join("a.plist");
    let binary = dir.path().join("b.plist");
    fs::write(&xml, XML).unwrap();
    fs::write(&binary, BINARY).unwrap();
    let mut cache = Cache::default();
    let parsed = cache.scan(roots()).await;
    assert_eq!(parsed[0].data, parsed[1].data);
    let data = parsed[0].data.as_ref().unwrap();
    assert!(data.run_at_load && data.keep_alive);
    assert_eq!(data.program.as_deref(), Some("/opt/homebrew/bin/redis-server"));
    assert_eq!(data.stderr.as_deref(), Some("/example/redis.err"));
    let original_mtime = fs::metadata(&xml).unwrap().modified().unwrap();
    fs::write(&xml, b"corrupt").unwrap();
    fs::File::open(&xml).unwrap().set_modified(original_mtime).unwrap();
    assert_eq!(cache.scan(roots()).await[0].data.as_ref(), Some(data));
    fs::File::open(&xml)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(100))
        .unwrap();
    fs::remove_file(&binary).unwrap();
    let changed = cache.scan(roots()).await;
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].data, None);
    assert_eq!(changed[0].label, "a");
}

fn file(label: &str, keep_alive: bool, run_at_load: bool) -> Entry {
    Entry {
        system: false,
        label: label.into(),
        path: format!("/example/{label}.plist"),
        data: Some(plists::Plist {
            label: label.into(),
            program: Some("/example/program".into()),
            keep_alive,
            run_at_load,
            stderr: None,
            stdout: None,
        }),
    }
}

#[test]
fn maps_state_autostart_pid_since_and_failure_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stdout");
    fs::write(&path, b"old\na\nb\nc\nd\ne\n").unwrap();
    let mut files = [
        file("homebrew.mxcl.redis", true, false),
        file("org.example.agent", false, true),
        file("org.example.unloaded", false, true),
        file("homebrew.mxcl.mysql@8.4", false, false),
        file("malformed", false, false),
    ];
    files[3].data.as_mut().unwrap().stderr = Some(dir.path().join("missing").to_str().unwrap().into());
    files[3].data.as_mut().unwrap().stdout = Some(path.to_str().unwrap().into());
    files[4].data = None;
    let snapshot = Snapshot {
        processes: vec![crate::Process {
            pid: 742,
            ppid: 1,
            user: "tester".into(),
            name: "redis-server".into(),
            command: "redis-server".into(),
            start_time: 86401,
            rss_bytes: Some(16384),
            state: 'S',
            cpu_percent: None,
            mem_percent: None,
            threads: 1,
            nice: 0,
        }],
        ..Snapshot::default()
    };
    let list = ServiceList {
        services: merge(
            "gui/501",
            parse_list(LIST),
            parse_disabled(DISABLED),
            files.iter(),
            Some(&snapshot),
            None,
            &mut Tails::default(),
        ),
    };
    let get = |label| list.get(&format!("gui/501/{label}")).unwrap();
    let redis = get("homebrew.mxcl.redis");
    assert_eq!(
        (&*redis.name, redis.autostart, redis.active.clone()),
        ("redis", Autostart::Enabled, ActiveState::Active)
    );
    assert_eq!((redis.main_pid, redis.memory), (Some(742), Some(16384)));
    // Local time zone, so only the shape is fixed: `1970-01-0x hh:mm:ss`.
    let since = redis.since_text.as_deref().unwrap();
    assert!(since.starts_with("1970-01-0") && since.len() == 19 && !since.contains("UTC"), "{since}");
    let mysql = get("homebrew.mxcl.mysql@8.4");
    assert_eq!((&*mysql.name, mysql.autostart), ("mysql@8.4", Autostart::Manual));
    assert_eq!(mysql.last_error.as_deref(), Some("上次退出码 78\na\nb\nc\nd\ne"));
    assert_eq!(get("org.example.agent").autostart, Autostart::Disabled);
    assert_eq!(get("org.example.unloaded").autostart, Autostart::Enabled);
    assert_eq!(get("org.example.unloaded").sub, "未加载");
    assert_eq!(get("org.example.unknown").sub, "状态未知");
    assert_eq!(get("malformed").sub, "状态未知");
    let broken = merge(
        "gui/501",
        Listing::default(),
        None,
        files.iter(),
        None,
        Some(&list),
        &mut Tails::default(),
    );
    assert_eq!(broken.len(), list.services.len());
    assert!(
        broken
            .iter()
            .all(|service| service.sub == "状态未知" && service.autostart == Autostart::Unknown)
    );
}

#[test]
fn bounded_log_tail_handles_rotation_missing_binary_and_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    assert_eq!(log_tail(&path), None);
    assert_eq!(log_tail(dir.path()), None);
    fs::write(&path, [255; 65536]).unwrap();
    assert_eq!(log_tail(&path).unwrap().chars().count(), 16384);
    fs::rename(&path, dir.path().join("old")).unwrap();
    fs::write(&path, b"new\n").unwrap();
    assert_eq!(log_tail(&path).as_deref(), Some("new"));
}

#[tokio::test]
async fn actions_validate_membership_and_record_argv_or_sudo_without_execution() {
    use ServiceAction::*;
    for domain in ["gui/501", "system"] {
        let target = format!("{domain}/homebrew.mxcl.redis");
        for (action, keep, loaded, args) in [
            (Start, false, true, vec!["kickstart", &target]),
            (Restart, true, true, vec!["kickstart", "-k", &target]),
            (Stop, false, true, vec!["kill", "TERM", &target]),
            (Stop, true, true, vec!["bootout", &target]),
            (
                Start,
                true,
                false,
                vec!["bootstrap", domain, "/example/it's redis.plist"],
            ),
            (
                Start,
                false,
                false,
                vec!["bootstrap", domain, "/example/it's redis.plist"],
            ),
            (Enable, true, true, vec!["enable", &target]),
            (Disable, true, true, vec!["disable", &target]),
        ] {
            let mut entry = file("homebrew.mxcl.redis", keep, false);
            entry.path = "/example/it's redis.plist".into();
            let listing = if loaded {
                parse_list(LIST)
            } else {
                parse_list("PID Status Label")
            };
            let list = ServiceList {
                services: merge(
                    domain,
                    listing,
                    Some(HashMap::new()),
                    std::iter::once(&entry),
                    None,
                    None,
                    &mut Tails::default(),
                ),
            };
            let expected = Command::new(std::iter::once("launchctl").chain(args.iter().copied())).unwrap();
            assert!(validated(&list, "gui/501/not-listed", action, &expected).is_none());
            assert!(validated(&list, "homebrew.mxcl.redis", action, &expected).is_none());
            assert!(validated(&list, &target, action, &Command::new(["launchctl", "other"]).unwrap()).is_none());
            let (system, words) = validated(&list, &target, action, &expected).unwrap();
            let result = dispatch(system, words, |cmd| async move {
                assert!(!system, "system operations must never invoke the runner");
                assert_eq!(cmd.as_std().get_program(), "/bin/launchctl");
                assert!(cmd.as_std().get_args().eq(args.iter().map(std::ffi::OsStr::new)));
                Err(std::io::Error::other("recorded only"))
            })
            .await;
            if system {
                assert_eq!(
                    result,
                    ActionOutcome::Terminal(Plan::Terminal {
                        line: expected.prefixed(&["sudo"]).line()
                    })
                );
            } else {
                assert_eq!(result, ActionOutcome::Error("recorded only".into()));
            }
        }
    }
    for target in [
        "gui/501/extra/job",
        "system/",
        "system/--",
        "system/line\nfeed",
        "other/svc",
    ] {
        let job = Job {
            target: target.into(),
            plist: None,
            log: None,
            loaded: Some(true),
            keep_alive: Some(false),
        };
        assert!(job.command(Restart).is_err());
    }
}
