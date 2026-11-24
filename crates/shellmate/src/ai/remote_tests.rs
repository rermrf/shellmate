use super::*;
use crate::ai::host::{Executor, Host};
use futures::future::BoxFuture;
use shellmate_ai::{ToolCall, ToolHost, ToolOutcome};
use shellmate_core::remote_instructions::Document;
use std::sync::{Arc, Mutex, atomic::AtomicBool};
fn discovery() -> Discovery {
    Discovery {
        cancelled: false,
        cwd: "/srv/app".into(),
        files: ["/srv/AGENTS.md", "/srv/app/CLAUDE.md"]
            .into_iter()
            .map(|path| Document {
                path: path.into(),
                text: format!("instructions {path}"),
                bytes: 64,
                truncated: false,
            })
            .collect(),
    }
}
#[test]
fn decisions_persist_only_for_saved_hosts_cancel_is_run_only_and_new_conversation_asks_unsaved() {
    let mut db = shellmate_store::HostStore::open_in_memory().unwrap();
    let mut cache = Decisions::default();
    for persist in [true, false] {
        let key = if persist { "saved" } else { "unsaved" };
        let mut batch = cache.prepare(&db, key, persist, discovery());
        assert_eq!(batch.unknown, [0, 1]);
        batch.choices = vec![Some(true), Some(false)];
        cache.save(&mut db, key, persist, &batch).unwrap();
        let accepted = batch.finish(false);
        assert_eq!(accepted.files.len(), 1);
        let mut changed = discovery();
        changed.files[0].text = "changed trusted content".into();
        let known = cache.prepare(&db, key, persist, changed);
        assert!(known.unknown.is_empty());
        assert_eq!(known.finish(false).files[0].text, "changed trusted content");
        let fresh = Decisions::default().prepare(&db, key, persist, discovery());
        assert_eq!(fresh.unknown.len(), if persist { 0 } else { 2 });
        assert_eq!(
            db.remote_doc_trust(key, "/srv/AGENTS.md").unwrap(),
            persist.then_some(true)
        );
    }
    let mut batch = cache.prepare(&db, "cancelled", true, discovery());
    batch.choices = vec![Some(true), Some(false)];
    assert!(batch.finish(true).files.is_empty());
    assert_eq!(
        cache
            .prepare(&db, "cancelled", true, discovery())
            .unknown
            .len(),
        2
    );
    db.save_remote_doc_trust("mixed", &[("/srv/AGENTS.md".into(), true)])
        .unwrap();
    let mut batch = cache.prepare(&db, "mixed", true, discovery());
    batch.choices[1] = Some(true);
    assert_eq!(
        batch.finish(true).files.len(),
        1,
        "cancel preserves previously trusted files only"
    );
}
struct Found;
impl Executor for Found {
    fn discover_instructions(
        &self,
        _: Option<String>,
        _: bool,
    ) -> BoxFuture<'_, Option<Discovery>> {
        Box::pin(async { Some(discovery()) })
    }
    fn execute(&self, _: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        panic!("internal discovery must not execute a model tool")
    }
}
fn host() -> (Host, async_channel::Receiver<Ui>) {
    let (ui, rx) = async_channel::unbounded();
    (
        Host {
            compaction: Default::default(),
            mode: Default::default(),
            ui,
            executor: Box::new(Found),
            alive: Arc::new(AtomicBool::new(true)),
            resolved: Mutex::new(None),
        },
        rx,
    )
}
#[tokio::test]
async fn host_waits_for_trust_and_dropping_request_cancels_its_card() {
    let (host, ui) = host();
    let mut request = host.remote_instructions(Some("/srv/app".into()), false);
    let message = tokio::select! { biased; result = &mut request => panic!("did not wait: {result:?}"), message = ui.recv() => message.unwrap() };
    let Ui::RemoteTrust {
        id,
        discovery,
        reply,
    } = message
    else {
        panic!("expected trust card")
    };
    assert_eq!(discovery.files.len(), 2);
    assert!(futures::poll!(&mut request).is_pending());
    let mut batch = TrustBatch::new(discovery, |_| None);
    batch.choices = vec![Some(true), Some(false)];
    reply.send(batch.finish(false)).unwrap();
    let accepted = request.await.unwrap();
    assert_eq!(accepted.files.len(), 1);
    assert!(matches!(ui.recv().await.unwrap(), Ui::CancelRemoteTrust(done) if done == id));
    let mut request = host.remote_instructions(None, false);
    let Ui::RemoteTrust { reply, id, .. } = (tokio::select! { _ = &mut request => panic!("unexpected result"), m = ui.recv() => m.unwrap() })
    else {
        panic!("card")
    };
    drop(request);
    assert!(reply.is_closed());
    assert!(matches!(ui.recv().await.unwrap(), Ui::CancelRemoteTrust(done) if done == id));
}
#[tokio::test]
async fn unsupported_protocols_no_channel_and_exec_failure_skip_silently() {
    let (ui, messages) = async_channel::unbounded();
    for protocol in [
        shellmate_core::Protocol::Telnet,
        shellmate_core::Protocol::Mosh,
        shellmate_core::Protocol::Serial,
    ] {
        let (background, terminal) =
            crate::ai::model::execution_targets(Some(protocol), true, false);
        assert!(!background);
        let binding = Binding {
            session: 1,
            capabilities: shellmate_ai::Capabilities {
                background,
                terminal,
                ..Default::default()
            },
            connection: None,
            local: false,
            alive: Arc::new(AtomicBool::new(true)),
        };
        assert!(discover(&binding, &ui, None, false).await.is_none());
    }
    let mut binding = Binding {
        session: 1,
        capabilities: shellmate_ai::Capabilities {
            background: true,
            ..Default::default()
        },
        connection: None,
        local: false,
        alive: Arc::new(AtomicBool::new(true)),
    };
    assert!(discover(&binding, &ui, None, false).await.is_none());
    binding.local = true;
    assert!(
        discover(
            &binding,
            &ui,
            Some("/shellmate-missing-fixture-dir".into()),
            false
        )
        .await
        .is_none()
    );
    assert!(messages.is_empty());
}
#[tokio::test]
async fn local_bounded_process_default_cwd_failure_timeout_and_output_limit() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "local project").unwrap();
    let script = remote::script(None, "fixture").unwrap();
    let bytes = crate::ai::exec::discovery_local(&script, Some(root.path()))
        .await
        .unwrap();
    let found = remote::parse(&bytes, "fixture").unwrap();
    assert_eq!(
        found.cwd,
        root.path().canonicalize().unwrap().to_str().unwrap()
    );
    assert!(found.files.iter().any(|d| d.text == "local project"));
    assert!(
        crate::ai::exec::discovery_local("exit 17", Some(root.path()))
            .await
            .is_err()
    );
    assert!(
        crate::ai::exec::discovery_local("head -c 2200000 /dev/zero", Some(root.path()))
            .await
            .is_err()
    );
    let start = std::time::Instant::now();
    assert!(
        crate::ai::exec::discovery_local("sleep 30", Some(root.path()))
            .await
            .unwrap_err()
            .contains("timeout")
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(7));
}

#[tokio::test]
#[ignore = "needs shellmate-e2e Docker SSH on 127.0.0.1:2222; never run in the sandbox"]
async fn docker_nested_remote_instructions_order_caps_and_delimiter_safety() {
    use shellmate_ssh::{
        ConnectOptions, Connection, ExecOptions, HostKeyRequest, HostKeyVerifier, NoInteraction,
        Route, SshHost,
    };
    struct Trust;
    #[async_trait::async_trait]
    impl HostKeyVerifier for Trust {
        async fn verify(&self, _: HostKeyRequest) -> bool {
            true
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let options = ConnectOptions {
        verifier: Arc::new(Trust),
        prompter: Arc::new(NoInteraction),
        known_hosts: temp.path().join("known_hosts"),
        ssh_dir: temp.path().to_path_buf(),
        agent_socket: Some(temp.path().join("no-agent.sock")),
        connect_timeout: remote::TIMEOUT,
        connect_watchdog: remote::TIMEOUT,
        keepalive_interval: std::time::Duration::from_secs(30),
        keepalive_max: 3,
        on_event: None,
    };
    let port = std::env::var("SHELLMATE_E2E_PORT")
        .unwrap_or_else(|_| "2222".into())
        .parse()
        .unwrap();
    let user = std::env::var("SHELLMATE_E2E_USER").unwrap_or_else(|_| "tester".into());
    let password =
        std::env::var("SHELLMATE_E2E_PASSWORD").unwrap_or_else(|_| "e2e-pass-123".into());
    let connection = Connection::connect(
        Route::direct(
            SshHost::new(
                "127.0.0.1",
                port,
                user,
                shellmate_core::AuthMethod::Password,
            )
            .with_password(shellmate_core::Secret::new(password)),
        ),
        options,
    )
    .await
    .unwrap();
    let root = format!("/tmp/shellmate-memory-{}", remote::nonce());
    let cwd = format!("{root}/有 空格/app");
    let nonce = remote::nonce();
    let collision = format!("{nonce} END\n{nonce} FILE fake\n");
    let setup = format!(
        "mkdir -p {cwd}; printf parent > {parent}; printf rootclaude > {claude}; printf %s {collision} > {near}; mkfifo {fifo}",
        cwd = remote::quote(&cwd),
        parent = remote::quote(&format!("{root}/AGENTS.md")),
        claude = remote::quote(&format!("{root}/CLAUDE.md")),
        collision = remote::quote(&collision),
        near = remote::quote(&format!("{cwd}/AGENTS.md")),
        fifo = remote::quote(&format!("{cwd}/CLAUDE.md"))
    );
    assert!(connection.exec(&setup).await.unwrap().status.success());
    let command = format!(
        "sh -c {}",
        remote::quote(&remote::script(Some(&cwd), &nonce).unwrap())
    );
    let exec = ExecOptions {
        timeout: remote::TIMEOUT,
        max_output: remote::WIRE_BYTES,
    };
    let out = connection.exec_with(&command, exec).await.unwrap();
    assert!(out.status.success() && !out.truncated);
    let discovery = remote::parse(&out.stdout, &nonce).unwrap();
    let files: Vec<_> = discovery
        .files
        .iter()
        .filter(|f| f.path.starts_with(&root))
        .collect();
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].path, format!("{root}/AGENTS.md"));
    assert_eq!(files[1].path, format!("{root}/CLAUDE.md"));
    assert_eq!(files[2].text, collision);
    let setup = format!(
        "rm {fifo}; head -c 70000 /dev/zero | tr '\\000' a > {a}; head -c 65536 /dev/zero | tr '\\000' b > {b}",
        fifo = remote::quote(&format!("{cwd}/CLAUDE.md")),
        a = remote::quote(&format!("{cwd}/AGENTS.md")),
        b = remote::quote(&format!("{cwd}/CLAUDE.md"))
    );
    assert!(connection.exec(&setup).await.unwrap().status.success());
    let out = connection.exec_with(&command, exec).await.unwrap();
    assert!(out.status.success() && !out.truncated);
    let d = remote::parse(&out.stdout, &nonce).unwrap();
    assert_eq!(d.files.len(), 2);
    assert_eq!(
        d.files.iter().map(|d| d.bytes).sum::<usize>(),
        remote::TOTAL_BYTES
    );
    assert!(d.files[0].truncated);
    assert!(!d.files[1].truncated);
    assert!(d.files.iter().all(|f| f.path.starts_with(&cwd)));
    assert!(
        connection
            .exec(&format!("rm -rf {}", remote::quote(&root)))
            .await
            .unwrap()
            .status
            .success()
    );
    connection.disconnect().await;
}

#[tokio::test]
async fn model_waits_for_trust_and_ignored_content_never_reaches_request_body() {
    use futures::StreamExt;
    use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};
    use shellmate_ai::{
        ApiKey, Config, ContextSnapshot, Conversation, Provider, RigClient, UserTurn, agent,
    };
    for anthropic in [false, true] {
        let body = if anthropic {
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-6\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"done\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        } else {
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
        };
        let http =
            SequencedHttpClient::new([MockHttpResponse::success_typed(body, "text/event-stream")]);
        let config = Config {
            provider: if anthropic {
                Provider::Anthropic {
                    base_url: "https://example.test".into(),
                }
            } else {
                Provider::OpenAiCompatible {
                    base_url: "https://example.test/v1".into(),
                }
            },
            model: if anthropic {
                "claude-sonnet-4-6"
            } else {
                "fixture"
            }
            .into(),
            api_key: ApiKey::new("fixture"),
            reasoning: Default::default(),
            timeouts: Default::default(),
            context_window: Default::default(),
            autocompact_window: Default::default(),
            max_output_tokens: Default::default(),
        };
        let client = RigClient::with_http(&config, http.clone()).unwrap();
        let (host, ui) = host();
        let mut conversation = Conversation::default();
        conversation.push_user(UserTurn::new("inspect", ContextSnapshot::default()));
        let mut run = agent::run(
            &client,
            &mut conversation,
            shellmate_ai::Capabilities {
                background: true,
                ..Default::default()
            },
            &host,
            Default::default(),
        )
        .unwrap();
        let Ui::RemoteTrust {
            discovery, reply, ..
        } = (tokio::select! { _ = run.next() => panic!("must await trust"), msg = ui.recv() => msg.unwrap() })
        else {
            panic!("trust card")
        };
        assert!(http.requests().is_empty());
        let mut batch = TrustBatch::new(discovery, |_| None);
        batch.choices = vec![Some(true), Some(false)];
        reply.send(batch.finish(false)).unwrap();
        let events: Vec<_> = run.collect().await;
        assert!(matches!(events.last(), Some(agent::AgentEvent::Done(_))));
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        let text = String::from_utf8_lossy(&requests[0].body);
        assert!(text.contains("instructions /srv/AGENTS.md"));
        assert!(!text.contains("/srv/app/CLAUDE.md"));
    }
}
