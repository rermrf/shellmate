use super::*;
struct Sessions {
    inner: Host,
    requests: Mutex<Vec<String>>,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
}
impl ToolHost for Sessions {
    fn sessions_enabled(&self) -> bool {
        true
    }
    fn model_slots(&self) -> Option<std::sync::Arc<tokio::sync::Semaphore>> {
        Some(self.slots.clone())
    }
    fn run(&self, call: ToolCall) -> BoxFuture<'_, ToolOutcome> {
        self.inner.run(call)
    }
    fn local_tool(
        &self,
        tool: agent::LocalTool,
        _: agent::ExecLimits,
    ) -> BoxFuture<'_, agent::LocalResult> {
        Box::pin(async move {
            assert!(matches!(tool, agent::LocalTool::Sessions(_)));
            self.requests.lock().unwrap().push("sessions".into());
            agent::LocalResult::text("fixture sessions")
        })
    }
}
#[tokio::test]
async fn both_provider_wires_keep_targets_and_continue_after_one_terminal_disconnects() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(a, &[("discover", "list_sessions", json!({}))], "", false),
            response(
                a,
                &[
                    (
                        "left",
                        "run_command",
                        json!({"command":"pwd","target":"background","session_id":"local-ref"}),
                    ),
                    (
                        "right",
                        "run_command",
                        json!({"command":"pwd","target":"background","session_id":"remote-ref"}),
                    ),
                ],
                "",
                false,
            ),
            response(a, &[], "other target remains usable", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let host = Sessions {
            inner: Host::new([ToolOutcome::Disconnected, completed("remote-result")]),
            requests: Default::default(),
            slots: slots.clone(),
        };
        let mut log = conversation();
        let mut stream = agent::run(
            &client,
            &mut log,
            Capabilities::default(),
            &host,
            fast_limits(),
        )
        .unwrap();
        while let Some(event) = stream.next().await {
            if let AgentEvent::Error(e) = event {
                panic!("{e}");
            }
        }
        drop(stream);
        assert_eq!(host.requests.lock().unwrap().len(), 1);
        let calls = host.inner.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|c| c.session_id.as_deref())
                .collect::<Vec<_>>(),
            [Some("local-ref"), Some("remote-ref")]
        );
        let bodies = bodies(&http);
        assert_eq!(bodies.len(), 3);
        let schema = bodies[0]["tools"].to_string();
        for name in [
            "list_sessions",
            "read_terminal",
            "session_id",
        ] {
            assert!(schema.contains(name), "{schema}");
        }
        assert!(!schema.contains("compare_directories"));
        assert!(tool_result(&bodies[2], a, "right").contains("remote-result"));
        assert_eq!(slots.available_permits(), 1);
    }
}

#[tokio::test]
async fn channels_are_chosen_per_call_independently_of_task_waiting_on_both_wires() {
    for a in [false, true] {
        let calls = [
            ("visible-wait", "run_command", json!({"command":"ls","session_id":"first","target":"terminal","run_in_background":false})),
            ("visible-async", "run_command", json!({"command":"make test","session_id":"second","target":"terminal","run_in_background":true})),
            ("separate-wait", "run_command", json!({"command":"ls","session_id":"first","target":"background","run_in_background":false})),
            ("separate-async", "run_command", json!({"command":"make check","session_id":"second","target":"background","run_in_background":true})),
        ];
        let http = SequencedHttpClient::new([
            response(a, &calls, "", false), response(a, &[], "done", false)
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Sessions {
            inner: Host::new((0..4).map(|_| completed("result"))), requests: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        };
        let mut log = Conversation::default();
        log.push_user(UserTurn::new("在第1和第2个终端执行 ls，让后台任务继续，然后通过独立通道检查文件", Default::default()));
        let events = agent::run(&client, &mut log, Capabilities::default(), &host, fast_limits()).unwrap().collect::<Vec<_>>().await;
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error(_))));
        let seen = host.inner.calls.lock().unwrap();
        assert_eq!(seen.iter().map(|c| (c.target, c.run_in_background)).collect::<Vec<_>>(), [
            (Target::Terminal, false), (Target::Terminal, true),
            (Target::Background, false), (Target::Background, true),
        ]);
        let requests = bodies(&http);
        let tools = &requests[0]["tools"];
        let command = tools.as_array().unwrap().iter().map(|t| if a {t} else {&t["function"]}).find(|t| t["name"] == "run_command").unwrap();
        let schema = &command[if a {"input_schema"} else {"parameters"}];
        assert_eq!(schema["properties"]["target"]["enum"], json!(["terminal", "background"]));
        assert_eq!(schema["required"], json!(["command", "target"]));
        let system = requests[0].to_string();
        assert!(system.contains("无需加入操作范围"));
        assert!(system.contains("序号对应对话中用户指向的那次列表"));
        assert!(system.contains("普通检查、日期计算、ping、sleep 等辅助命令优先 background"));
        assert!(system.contains("日志读取成功不代表原命令成功"));
        assert!(system.contains("不扫描相邻项目"));
    }
}

#[tokio::test]
async fn service_intent_is_async_preserves_channel_and_rejects_invalid_readiness_on_both_wires() {
    for a in [false, true] {
        let http = SequencedHttpClient::new([
            response(
                a,
                &[
                    (
                        "service",
                        "run_command",
                        json!({"command":"npm run dev","target":"background","service":{"ready_pattern":"Local:"}}),
                    ),
                    (
                        "visible",
                        "run_command",
                        json!({"command":"npm run dev","target":"terminal","service":{}}),
                    ),
                    (
                        "invalid",
                        "run_command",
                        json!({"command":"npm run dev","target":"background","service":{"ready_pattern":"","startup_timeout_secs":1000}}),
                    ),
                ],
                "",
                false,
            ),
            response(a, &[], "服务已启动，继续检查访问地址", false),
        ]);
        let client = RigClient::with_http(&config(a), http.clone()).unwrap();
        let host = Host::new([completed("ready"), completed("pending")]);
        let mut log = conversation();
        let events = agent::run(&client, &mut log, capabilities(), &host, fast_limits())
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(|e| !matches!(e, AgentEvent::Error(_))));
        let calls = host.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|c| c.run_in_background && c.service.is_some())
        );
        assert_eq!(calls[0].target, Target::Background);
        assert_eq!(calls[1].target, Target::Terminal);
        assert_eq!(calls[0].service.as_ref().unwrap().startup_timeout_secs, 10);
        assert!(tool_result(&bodies(&http)[1], a, "invalid").contains("service"));
        let text = bodies(&http)[0].to_string();
        assert!(text.contains("常驻服务退出"));
        assert!(text.contains("ready_pattern"));
    }
}

#[tokio::test]
async fn retired_comparison_is_history_only_and_generic_commands_still_reach_both_targets() {
    use super::responses_tests::{responses_config, wire};
    for protocol in 0..3 {
        let a = protocol == 1;
        let cfg = if protocol == 2 {
            responses_config()
        } else {
            config(a)
        };
        let reply = |calls: &[(&str, &str, Value)], text: &str| {
            if protocol == 2 {
                let mut items = calls
                    .iter()
                    .map(|(id, name, args)| wire::call(id, name, args.clone()))
                    .collect::<Vec<_>>();
                if !text.is_empty() {
                    items.push(wire::message("answer", text, "final_answer"));
                }
                wire::reply(items)
            } else {
                response(a, calls, text, false)
            }
        };
        let args = json!({"left":{"session_id":"local-ref","path":"/project"},"right":{"session_id":"remote-ref","path":"/srv/app"}});
        let http = SequencedHttpClient::new([
            reply(&[("discover", "list_sessions", json!({}))], ""),
            reply(&[("retired", "compare_directories", args.clone())], ""),
            reply(
                &[
                    (
                        "left",
                        "run_command",
                        json!({"session_id":"local-ref","command":"ls -a","target":"background","cwd":"/project"}),
                    ),
                    (
                        "right",
                        "run_command",
                        json!({"session_id":"remote-ref","command":"ls -a","target":"background","cwd":"/srv/app"}),
                    ),
                ],
                "",
            ),
            reply(&[], "两侧清单已读取，需要进一步核实内容差异。"),
        ]);
        let client = RigClient::with_http(&cfg, http.clone()).unwrap();
        let host = Sessions {
            inner: Host::new([completed("LOCAL_FILE"), completed("REMOTE_FILE")]),
            requests: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        };
        let mut log = conversation();
        log.append(Entry::Step(Step {
            message_id: 1,
            call: LoggedCall {
                id: "old-compare".into(),
                name: "compare_directories".into(),
                args_json: args,
            },
            result: LoggedResult {
                attachments: vec![],
                subagent: None,
                inline: "HISTORICAL_RESULT: config differs".into(),
                output_id: Some("saved-comparison".into()),
                cleared: false,
                is_error: false,
            },
            display: None,
        }));
        log.push_user(UserTurn::new(
            "检查第1和第2个终端的项目目录",
            Default::default(),
        ));
        let events = agent::run(
            &client,
            &mut log,
            Capabilities::default(),
            &host,
            fast_limits(),
        )
        .unwrap()
        .collect::<Vec<_>>()
        .await;
        assert!(matches!(events.last(), Some(AgentEvent::Done(_))));
        let calls = host.inner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls
                .iter()
                .map(|c| (c.session_id.as_deref(), c.target, c.cwd.as_deref()))
                .collect::<Vec<_>>(),
            [
                (Some("local-ref"), Target::Background, Some("/project")),
                (Some("remote-ref"), Target::Background, Some("/srv/app"))
            ]
        );
        assert_eq!(
            host.requests.lock().unwrap().len(),
            1,
            "retired tool never reaches the host"
        );
        let requests = bodies(&http);
        assert_eq!(requests.len(), 4);
        for request in &requests {
            let tools = request["tools"].to_string();
            assert!(!tools.contains("compare_directories"));
            for name in ["list_sessions", "read_terminal", "run_command"] {
                assert!(tools.contains(name));
            }
        }
        assert!(requests[0].to_string().contains("HISTORICAL_RESULT"));
        assert!(
            requests[3].to_string().contains("LOCAL_FILE")
                && requests[3].to_string().contains("REMOTE_FILE")
        );
        assert!(log.entries().iter().any(|e|matches!(e,Entry::Step(s)if s.call.id=="retired" && s.result.is_error && s.result.inline.contains("未知工具"))));
        assert!(log.entries().iter().any(|e|matches!(e,Entry::Step(s)if s.call.id=="old-compare" && s.result.inline.contains("HISTORICAL_RESULT") && s.result.output_id.as_deref()==Some("saved-comparison"))));
    }
}

#[tokio::test]
async fn interactive_terminal_tools_round_trip_on_all_three_protocols_and_child_source_is_preserved(){
    use shellmate_ai::agent::{LocalTool,LocalResult,ExecLimits};
    struct Host{requests:Mutex<Vec<(String,Option<SubagentRef>,String)>>}
    impl ToolHost for Host{
        fn sessions_enabled(&self)->bool{true}
        fn run(&self,_:ToolCall)->BoxFuture<'_,ToolOutcome>{Box::pin(async{panic!("TUI input must not execute as a shell command")})}
        fn local_tool_from(&self,tool:LocalTool,_:ExecLimits,source:Option<SubagentRef>,id:&str)->BoxFuture<'_,LocalResult>{
            let id=id.to_owned();Box::pin(async move{
                let name=match tool{LocalTool::Sessions(shellmate_ai::session_tools::Request::Create(r))=>{assert_eq!(r.command,"vim example");"create"},LocalTool::Sessions(shellmate_ai::session_tools::Request::Manage(r))=>{assert_eq!(r.session_id,"target");"manage"},LocalTool::Sessions(shellmate_ai::session_tools::Request::Read(r))=>{assert!(r.screen);assert_eq!(r.session_id,"target");"read"},LocalTool::Sessions(shellmate_ai::session_tools::Request::Input(i))=>{assert_eq!(i.screen_id,"observed");"input"},LocalTool::Sessions(shellmate_ai::session_tools::Request::Wait(w))=>{assert_eq!(w.text.as_deref(),Some("selected"));"wait"},_=>panic!("unexpected request")};
                self.requests.lock().unwrap().push((name.into(),source,id));LocalResult::text("observed selected")
            })
        }
    }
    for protocol in 0..3{for child in [false,true]{
        let calls=[("create","create_terminal",json!({"session_id":"target","command":"vim example","purpose":"编辑"})),("manage","manage_terminal",json!({"session_id":"target","action":"resize","columns":100,"rows":30,"purpose":"调整画面"})),("read","read_terminal",json!({"session_id":"target","screen":true})),("input","terminal_input",json!({"session_id":"target","screen_id":"observed","purpose":"选择菜单项","guards":[{"region":{"row":1,"column":0,"rows":1,"columns":20},"text":"菜单"}],"observe":{"text":"selected","timeout_ms":500},"actions":[{"type":"key","key":"6"}]})),("wait","wait_terminal",json!({"session_id":"target","screen_id":"observed","text":"selected","absent_text":"loading"}))];
        let cfg=if protocol==2{super::responses_tests::responses_config()}else{config(protocol==1)};
        let first=if protocol==2{super::responses_tests::wire::reply(calls.iter().map(|(id,name,args)|super::responses_tests::wire::call(id,name,args.clone())).collect())}else{response(protocol==1,&calls,"",false)};
        let last=if protocol==2{super::responses_tests::wire::reply(vec![super::responses_tests::wire::message("end","完成","final_answer")])}else{response(protocol==1,&[],"完成",false)};
        let http=SequencedHttpClient::new([first,last]);let client=RigClient::with_http(&cfg,http.clone()).unwrap();let host=Host{requests:Default::default()};
        let mut log=conversation();
        if child{
            let request=SubagentRequest{image_ids:vec![],session_ids:vec![],default_session_id:None,description:"交互任务".into(),prompt:"操作已有终端菜单".into(),kind:SubagentKind::General,background:true,model:None,reasoning:None};
            let context=shellmate_ai::subagent::SubagentContext{attachments:vec![],sent_at_ms:None,origin:None,id:"child".into(),snapshot:Default::default(),cwd:None};
            let out=shellmate_ai::subagent::run(&client,request,context,Default::default(),&host,fast_limits(),|_|{}).await;assert_eq!(out.status,SubagentStatus::Completed);
        }else{let events=agent::run(&client,&mut log,capabilities(),&host,fast_limits()).unwrap().collect::<Vec<_>>().await;assert!(matches!(events.last(),Some(AgentEvent::Done(_))));}
        let calls=host.requests.lock().unwrap();assert_eq!(calls.len(),5);assert!(calls.iter().all(|(_,source,_)|source.is_some()==child));assert_eq!(calls.iter().map(|(_,_,id)|id.as_str()).collect::<Vec<_>>(),["create","manage","read","input","wait"]);
        let requests=bodies(&http);assert!(requests[0]["tools"].to_string().contains("terminal_input"));assert!(requests[0]["tools"].to_string().contains("wait_terminal"));assert!(requests[1].to_string().contains("observed selected"));
    }}
}
