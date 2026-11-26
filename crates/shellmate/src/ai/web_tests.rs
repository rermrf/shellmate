use super::*;
use shellmate_store::{HostStore, MemoryStore};
use std::sync::atomic::{AtomicUsize, Ordering};
struct Mock {
    responses: Mutex<VecDeque<Result<Response, String>>>,
    requests: Mutex<Vec<(String, Option<serde_json::Value>, bool)>>,
}
impl Transport for Mock {
    fn request<'a>(
        &'a self,
        url: &'a str,
        body: Option<Vec<u8>>,
        key: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Response, String>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push((
                url.into(),
                body.map(|v| serde_json::from_slice(&v).unwrap()),
                key.is_some(),
            ));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected network request")
        })
    }
}
fn response(status: u16, body: &str, kind: &str) -> Result<Response, String> {
    Ok(Response {
        status,
        body: body.into(),
        url: "https://example.com/docs".into(),
        content_type: kind.into(),
    })
}
fn fixture(responses: Vec<Result<Response, String>>) -> (Service, Arc<Mock>) {
    let db = SharedStore::new(HostStore::open_in_memory().unwrap());
    db.borrow_mut().set_setting(ENABLED, "1").unwrap();
    let credentials = Arc::new(MemoryStore::default());
    credentials
        .set(
            &SecretKey::Account(KEY.into()),
            &shellmate_core::Secret::new("fixture-key-never-log"),
        )
        .unwrap();
    let mock = Arc::new(Mock {
        responses: Mutex::new(responses.into()),
        requests: Default::default(),
    });
    let mut service = Service::new(db, credentials);
    service.transport = mock.clone();
    (service, mock)
}
fn search() -> Request {
    Request::Search(Search {
        query: "nginx docs".into(),
        max_results: 5,
        include_domains: vec![],
    })
}
#[tokio::test]
async fn tavily_basic_is_primary_and_cache_avoids_duplicate_queries() {
    let (service, mock) = fixture(vec![response(
        200,
        r#"{"results":[{"title":"Official","url":"https://nginx.org/en/docs/","content":"documentation"}]}"#,
        "application/json",
    )]);
    let first = service.execute(search()).await.unwrap();
    let second = service.execute(search()).await.unwrap();
    assert!(
        matches!(first,ResultData::Search{provider,cached:false,results,..}if provider=="Tavily"&&results.len()==1)
    );
    assert!(matches!(second, ResultData::Search { cached: true, .. }));
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].2);
    let body = requests[0].1.as_ref().unwrap();
    assert_eq!(body["search_depth"], "basic");
    assert_eq!(body["auto_parameters"], false);
    assert!(!body.to_string().contains("fixture-key"));
}
#[tokio::test]
async fn quota_falls_back_once_without_forwarding_key_and_deduplicates_links() {
    let html = r#"<table><tr><td><a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fnginx.org%2Fen%2Fdocs%2F">Nginx docs</a></td></tr><tr><td class="result-snippet">Useful docs</td></tr><tr><a class="result-link" href="https://nginx.org/en/docs/">duplicate</a></tr><a class="result-link" href="javascript:alert(1)">bad</a></table>"#;
    let (service, mock) = fixture(vec![
        response(432, "quota", "application/json"),
        response(200, html, "text/html"),
    ]);
    let data = service.execute(search()).await.unwrap();
    assert!(
        matches!(data,ResultData::Search{provider,results,note:Some(_),..}if provider=="DuckDuckGo"&&results.len()==1&&results[0].snippet=="Useful docs")
    );
    let req = mock.requests.lock().unwrap();
    assert_eq!(req.len(), 2);
    assert!(!req[1].2 && req[1].1.is_none());
    assert!(req[1].0.starts_with("https://lite.duckduckgo.com/"));
}
#[tokio::test]
async fn invalid_key_disabled_fallback_and_empty_results_are_distinct() {
    let (s, m) = fixture(vec![response(401, "unauthorized", "application/json")]);
    assert!(s.execute(search()).await.unwrap_err().contains("Key"));
    assert_eq!(m.requests.lock().unwrap().len(), 1);
    let (s, m) = fixture(vec![response(429, "quota", "application/json")]);
    s.db.borrow_mut().set_setting(FALLBACK, "0").unwrap();
    assert!(s.execute(search()).await.is_err());
    assert_eq!(m.requests.lock().unwrap().len(), 1);
    let (s, m) = fixture(vec![response(200, "{\"results\":[]}", "application/json")]);
    assert!(
        matches!(s.execute(search()).await.unwrap(),ResultData::Search{results,..}if results.is_empty())
    );
    assert_eq!(m.requests.lock().unwrap().len(), 1);
    let (s, m) = fixture(vec![
        Err("network".into()),
        response(202, "bots use duckduckgo too", "text/html"),
    ]);
    assert!(s.execute(search()).await.unwrap_err().contains("验证"));
    assert_eq!(m.requests.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn fetch_preserves_code_links_and_chunks_unicode_without_refetch() {
    let body = format!(
        "<title>Docs &amp; Examples</title><body><nav>REMOVE NAV</nav><main><h1>安装</h1><p>{}</p><pre>cargo build\n  --release</pre><a href='/reference'>参考</a><script>REMOVE SCRIPT</script></main></body>",
        "中文正文内容".repeat(500)
    );
    let (s, m) = fixture(vec![response(200, &body, "text/html")]);
    let fetch = |offset| {
        Request::Fetch(Fetch {
            url: "https://example.com/docs".into(),
            offset,
            max_chars: 1000,
        })
    };
    let first = s.execute(fetch(0)).await.unwrap();
    let ResultData::Page {
        title,
        content,
        next_offset: Some(offset),
        total_chars,
        ..
    } = first
    else {
        panic!("page")
    };
    assert_eq!(title, "Docs & Examples");
    assert_eq!(content.chars().count(), 1000);
    assert!(total_chars > 2000);
    let last = s.execute(fetch(total_chars - 100)).await.unwrap();
    assert!(
        matches!(last,ResultData::Page{content,cached:true,next_offset:None,..}if content.contains("--release")&&content.contains("https://example.com/reference"))
    );
    assert_eq!(offset, 1000);
    assert_eq!(m.requests.lock().unwrap().len(), 1);
    let all = s.page_text("https://example.com/docs").unwrap();
    assert!(!all.contains("REMOVE"));
    assert!(all.contains("cargo build\n  --release"));
}
#[test]
fn web_addresses_reject_private_targets_and_encoded_redirect_destinations() {
    for url in [
        "file:///etc/passwd",
        "http://localhost/a",
        "http://127.0.0.1/",
        "http://2130706433/",
        "http://169.254.169.254/",
        "http://10.0.0.1/",
        "http://[::1]/",
        "http://[::ffff:127.0.0.1]/",
        "https://user:password@example.com",
        "http://host.local/",
        "http://100.64.0.1/",
    ] {
        assert!(public_url(url).is_err(), "{url}");
    }
    assert!(public_url("https://nginx.org/en/docs/#reference").is_ok());
    assert!(!public_ip("192.0.2.1".parse().unwrap()));
    assert!(!public_ip("2001:db8::1".parse().unwrap()));
    assert!(!domain_allowed(
        "https://nginx.org.evil.com/",
        &["nginx.org".into()]
    ));
    assert!(domain_allowed(
        "https://docs.nginx.org/",
        &["nginx.org".into()]
    ));
}
#[tokio::test]
async fn disabled_and_missing_key_do_not_make_network_calls() {
    let (s, m) = fixture(vec![]);
    s.db.borrow_mut().set_setting(ENABLED, "0").unwrap();
    assert!(s.execute(search()).await.is_err());
    assert!(m.requests.lock().unwrap().is_empty());
    s.db.borrow_mut().set_setting(ENABLED, "1").unwrap();
    s.credentials
        .delete(&SecretKey::Account(KEY.into()))
        .unwrap();
    assert!(s.execute(search()).await.unwrap_err().contains("保存"));
    assert!(m.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn dropping_request_releases_network_future_and_capacity() {
    struct Pending(Arc<AtomicUsize>);
    struct Guard(Arc<AtomicUsize>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    impl Transport for Pending {
        fn request<'a>(
            &'a self,
            _: &'a str,
            _: Option<Vec<u8>>,
            _: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Response, String>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                let _guard = Guard(self.0.clone());
                std::future::pending().await
            })
        }
    }
    let (mut s, _) = fixture(vec![]);
    let active = Arc::new(AtomicUsize::new(0));
    s.transport = Arc::new(Pending(active.clone()));
    let mut request = Box::pin(s.execute(Request::Fetch(Fetch {
        url: "https://example.com/".into(),
        offset: 0,
        max_chars: 1000,
    })));
    tokio::select! {_=&mut request=>panic!("pending"),_=tokio::task::yield_now()=>{}}
    assert_eq!(active.load(Ordering::SeqCst), 1);
    drop(request);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(s.slots.available_permits(), 3);
}

#[tokio::test]
async fn child_uses_shared_web_service_and_fetch_output_survives_history_reopen() {
    use shellmate_ai::{
        Conversation, Entry, LoggedCall, LoggedResult, Step, UserTurn,
        agent::{LocalTool, ReadOutput},
    };
    let (s, m) = fixture(vec![response(
        200,
        &"line of documentation\n".repeat(200),
        "text/plain",
    )]);
    let root = tempfile::tempdir().unwrap();
    let parent = crate::ai::tasks::Tasks::new(root.path().into(), "conversation".into());
    parent.test_web(Arc::new(s));
    let child = parent.child_tasks();
    assert!(child.web_enabled());
    let request = LocalTool::Web(Request::Fetch(Fetch {
        url: "https://example.com/docs".into(),
        offset: 0,
        max_chars: 1000,
    }));
    let result = child.local_tool(request.clone(), Default::default()).await;
    assert!(!result.is_error);
    let id = result.output_id.clone().unwrap();
    assert_eq!(
        std::fs::read_to_string(
            shellmate_core::ai_output::path(root.path(), "conversation", &id).unwrap()
        )
        .unwrap(),
        "line of documentation\n".repeat(200)
    );
    parent.local_tool(request, Default::default()).await;
    assert_eq!(m.requests.lock().unwrap().len(), 1);
    let mut log = Conversation::default();
    log.push_user(UserTurn::new("read docs", Default::default()));
    log.append(Entry::Step(Step {
        message_id: 1,
        call: LoggedCall {
            id: "fetch".into(),
            name: "web_fetch".into(),
            args_json: serde_json::json!({"url":"https://example.com/docs"}),
        },
        result: LoggedResult {
            inline: result.inline,
            output_id: Some(id.clone()),
            is_error: false,
            cleared: false,
            subagent: None,
        },
        display: None,
    }));
    let reopened = crate::ai::tasks::Tasks::new(root.path().into(), "conversation".into());
    reopened.remember_outputs(&log);
    let result = reopened
        .local_tool(
            LocalTool::ReadOutput(ReadOutput {
                id,
                offset_line: 0,
                limit_lines: Some(5),
                grep: None,
            }),
            Default::default(),
        )
        .await;
    assert!(!result.is_error);
    assert!(result.inline.contains("line of documentation"));
}

#[tokio::test]
async fn short_json_and_expired_page_offset_are_explicit() {
    let (s, m) = fixture(vec![response(
        200,
        "{\"status\":\"ok\"}",
        "application/json",
    )]);
    let first = s
        .execute(Request::Fetch(Fetch {
            url: "https://example.com/docs".into(),
            offset: 0,
            max_chars: 1000,
        }))
        .await
        .unwrap();
    assert!(matches!(first,ResultData::Page{content,..}if content.contains("status")));
    s.cache.lock().unwrap().clear();
    let error = s
        .execute(Request::Fetch(Fetch {
            url: "https://example.com/docs".into(),
            offset: 1,
            max_chars: 1000,
        }))
        .await
        .unwrap_err();
    assert!(error.contains("缓存已过期"));
    assert_eq!(m.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn urgent_input_cancels_web_tool_without_retry_or_waiting_for_timeout() {
    struct Hanging(Arc<AtomicUsize>);
    struct Active(Arc<AtomicUsize>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    impl Transport for Hanging {
        fn request<'a>(
            &'a self,
            _: &'a str,
            _: Option<Vec<u8>>,
            _: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Response, String>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                let _active = Active(self.0.clone());
                std::future::pending().await
            })
        }
    }
    let (mut s, _) = fixture(vec![]);
    let active = Arc::new(AtomicUsize::new(0));
    s.transport = Arc::new(Hanging(active.clone()));
    let root = tempfile::tempdir().unwrap();
    let tasks = crate::ai::tasks::Tasks::new(root.path().into(), "c".into());
    tasks.test_web(Arc::new(s));
    let mut tool = Box::pin(tasks.local_tool(
        shellmate_ai::agent::LocalTool::Web(Request::Fetch(Fetch {
            url: "https://example.com/".into(),
            offset: 0,
            max_chars: 1000,
        })),
        Default::default(),
    ));
    tokio::select! {_=&mut tool=>panic!("pending"),_=tokio::task::yield_now()=>{}}
    assert_eq!(active.load(Ordering::SeqCst), 1);
    tasks
        .control()
        .request(shellmate_ai::interaction::Intent::StopReply);
    let result = tokio::time::timeout(Duration::from_secs(1), tool)
        .await
        .unwrap();
    assert!(result.is_error && result.inline.contains("已取消"));
    assert_eq!(active.load(Ordering::SeqCst), 0);
}
