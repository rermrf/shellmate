use super::*;
use shellmate_ai::web::ResultData;
impl Drawer {
    #[cfg(debug_assertions)]
    pub(super) fn web_fixture(&mut self, scene: &str, cx: &mut Context<Self>) {
        use shellmate_ai::{Conversation, Entry, LoggedCall, LoggedResult, Step, UserTurn};
        self.state.model.conversation = Conversation::default();
        self.state.model.conversation.push_user(UserTurn::new(
            "查一下 nginx upstream timeout 的官方说明",
            Default::default(),
        ));
        let data = if scene == "ai-web-fetch" {
            ResultData::Page{url:"https://nginx.org/en/docs/http/ngx_http_proxy_module.html".into(),title:"Module ngx_http_proxy_module".into(),content:"# proxy_read_timeout\n\n设置从代理服务器读取响应的超时时间。该时间限制作用于两次连续读取之间，而不是整个响应传输。\n\n```nginx\nproxy_read_timeout 60s;\n```\n\n应先检查上游服务响应时间，避免仅通过增加超时掩盖问题。".into(),total_chars:220,offset:0,next_offset:None,fetched_at:crate::ai::now(),cached:false}
        } else {
            ResultData::Search{query:"nginx upstream timed out 官方文档".into(),provider:if scene=="ai-web-fallback"{"DuckDuckGo"}else{"Tavily"}.into(),results:vec![shellmate_ai::web::Hit{title:"Module ngx_http_proxy_module".into(),url:"https://nginx.org/en/docs/http/ngx_http_proxy_module.html".into(),snippet:"proxy_read_timeout 定义从代理服务器读取响应的超时，适用于两次连续读取操作之间。".into()},shellmate_ai::web::Hit{title:"NGINX upstream troubleshooting".into(),url:"https://docs.nginx.com/nginx/admin-guide/".into(),snippet:"检查上游服务器的连接、响应时间和应用日志，以区分连接超时与读取超时。".into()},shellmate_ai::web::Hit{title:"NGINX documentation".into(),url:"https://nginx.org/en/docs/".into(),snippet:"官方配置指令与管理指南。".into()}],searched_at:crate::ai::now(),cached:false,note:(scene=="ai-web-fallback").then(||"Tavily 额度受限，已使用备用搜索".into())}
        };
        let name = if scene == "ai-web-fetch" {
            "web_fetch"
        } else {
            "web_search"
        };
        self.state.model.conversation.append(Entry::Step(Step {
            message_id: 1,
            call: LoggedCall {
                id: "web-fixture".into(),
                name: name.into(),
                args_json: serde_json::json!({"query":"nginx upstream timeout"}),
            },
            result: LoggedResult {
                attachments: Vec::new(), inline: if scene == "ai-web-error" {
                    "Tavily API Key 无效或无权限，请检查设置".into()
                } else {
                    serde_json::to_string(&data).unwrap()
                },
                output_id: None,
                is_error: scene == "ai-web-error",
                cleared: false,
                subagent: None,
            },
            display: None,
        }));
        if scene == "ai-web-fetch" {
            self.state
                .tool_expanded
                .insert(self.tool_key("main", "web-fixture", false));
        }
        self.state.model.busy = false;
        cx.notify();
    }
    pub(super) fn web_content(
        &self,
        key: &str,
        data: &ResultData,
        expanded: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let p = cx.palette().clone();
        let mut body = div().min_w_0().flex().flex_col().gap(px(8.));
        match data {
            ResultData::Search {
                query: _,
                provider,
                results,
                searched_at,
                cached,
                note,
            } => {
                body = body.child(div().text_size(px(11.)).text_color(p.tx3).child(format!(
                    "{provider} · {} 条结果{} · {}",
                    results.len(),
                    if *cached { " · 缓存" } else { "" },
                    timing::timestamp(
                        searched_at.saturating_mul(1000),
                        timing::Moment::now().wall_ms,
                        false
                    )
                )));
                if let Some(note) = note {
                    body = body.child(
                        div()
                            .text_size(px(11.5))
                            .text_color(p.tx2)
                            .whitespace_normal()
                            .child(note.clone()),
                    );
                }
                if results.is_empty() {
                    body = body.child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.tx2)
                            .child("没有找到匹配结果，可以调整查询词。"),
                    );
                }
                for (index, hit) in results
                    .iter()
                    .take(if expanded { usize::MAX } else { 2 })
                    .enumerate()
                {
                    let safe = crate::ai::web::public_url(&hit.url).ok();
                    let domain = safe
                        .as_ref()
                        .and_then(|u| u.host_str())
                        .unwrap_or("网页")
                        .to_owned();
                    let url = hit.url.clone();
                    let mut row = div()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .p(px(10.))
                        .rounded(px(10.))
                        .bg(p.card)
                        .border_1()
                        .border_color(p.line)
                        .child(div().text_size(px(11.)).text_color(p.tx3).child(domain));
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("{key}:link:{index}")))
                            .text_size(px(12.5))
                            .text_color(p.acc)
                            .whitespace_normal()
                            .cursor_pointer()
                            .child(format!("{} ↗", hit.title))
                            .on_click(move |_, _, cx| {
                                if crate::ai::web::public_url(&url).is_ok() {
                                    cx.open_url(&url);
                                }
                            }),
                    );
                    let snippet = if expanded {
                        hit.snippet.clone()
                    } else {
                        hit.snippet.chars().take(180).collect()
                    };
                    row = row.child(
                        super::subagent_ui::selectable(
                            format!("{key}:snippet:{index}"),
                            &snippet,
                            w,
                            cx,
                        )
                        .text_size(px(12.))
                        .text_color(p.tx2)
                        .whitespace_normal(),
                    );
                    body = body.child(row);
                }
            }
            ResultData::Page {
                url,
                title,
                total_chars,
                offset,
                next_offset,
                fetched_at,
                cached,
                content,
            } => {
                let link = url.clone();
                body = body
                    .child(
                        div()
                            .id(SharedString::from(format!("{key}:url")))
                            .text_size(px(12.5))
                            .text_color(p.acc)
                            .whitespace_normal()
                            .cursor_pointer()
                            .child(format!("{title} ↗"))
                            .on_click(move |_, _, cx| {
                                if crate::ai::web::public_url(&link).is_ok() {
                                    cx.open_url(&link);
                                }
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(p.tx3)
                            .whitespace_normal()
                            .child(format!(
                                "正文 {} 字符 · {}{}",
                                total_chars,
                                timing::timestamp(
                                    fetched_at.saturating_mul(1000),
                                    timing::Moment::now().wall_ms,
                                    false
                                ),
                                if *cached { " · 缓存" } else { "" }
                            )),
                    );
                if next_offset.is_some() || *offset > 0 {
                    body = body.child(
                        div()
                            .text_size(px(11.))
                            .text_color(p.tx3)
                            .child("模型按片段读取；展开查看已保存的正文"),
                    );
                }
                if expanded {
                    if let Some(pane) = self.state.output_panes.get(key) {
                        body = body.child(pane.clone());
                    } else {
                        body = body.child(
                            super::subagent_ui::selectable(
                                format!("{key}:content"),
                                content,
                                w,
                                cx,
                            )
                            .text_size(px(12.))
                            .whitespace_normal(),
                        );
                    }
                }
            }
        }
        body
    }
}
