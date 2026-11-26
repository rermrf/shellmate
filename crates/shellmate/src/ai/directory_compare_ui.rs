use super::*;
impl Drawer {
    fn open_comparison_preview(
        &mut self,
        key: String,
        cursor: String,
        path: String,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.comparison_previews.remove(&key).is_some() {
            cx.notify();
            return;
        }
        let Some(r) = self.runtime.clone() else {
            self.comparison_previews
                .insert(key, "请关联原终端并继续此对话后查看文件差异".into());
            cx.notify();
            return;
        };
        let Some(runtime) = crate::runtime::get(cx) else {
            return;
        };
        let conversation = self.state.id.clone();
        self.comparison_previews
            .insert(key.clone(), "正在读取并核对两侧文件…".into());
        let job = runtime.spawn(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                crate::ai::directory_compare::text_diff(&r.router, &r.tasks, &cursor, &path),
            )
            .await
            .unwrap_or_else(|_| Err("读取超时，请重试".into()))
        });
        cx.spawn_in(w, async move |this, cx| {
            let text = match job.await {
                Ok(Ok(text)) => text,
                Ok(Err(e)) => e,
                Err(_) => "读取已取消".into(),
            };
            this.update_in(cx, |s, _, cx| {
                if s.state.id == conversation && s.comparison_previews.contains_key(&key) {
                    s.comparison_previews.insert(key, text);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
    pub(super) fn comparison_content(
        &self,
        key: &str,
        result: &str,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let value: serde_json::Value = serde_json::from_str(result).ok()?;
        let rows = value["differences"].as_array()?;
        let p = cx.palette().clone();
        let mut card = div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .text_size(px(12.));
        for side in ["left", "right"] {
            card = card.child(div().whitespace_normal().text_color(p.tx2).child(format!(
                "{}：{} · {}",
                if side == "left" { "左侧" } else { "右侧" },
                value[side]["label"].as_str().unwrap_or("终端"),
                value[format!("{side}_path")].as_str().unwrap_or_default()
            )));
        }
        card = card.child(
            div()
                .text_color(if value["scan_complete"] == true {
                    p.tx3
                } else {
                    p.warn
                })
                .child(if value["scan_complete"] == true {
                    "扫描已完成；差异以各文件采样时刻为准"
                } else {
                    "部分完成，继续比较后才能确认完整结果"
                }),
        );
        if let Some(collisions) = value["path_collisions"]
            .as_array()
            .filter(|v| !v.is_empty())
        {
            card = card.child(div().text_color(p.warn).whitespace_normal().child(format!(
                "有 {} 组路径仅大小写或 Unicode 形式不同，已分别列出，请勿视为同一路径。",
                collisions.len()
            )));
        }
        if let Some(bytes) = value["verified_bytes"].as_object() {
            card = card.child(div().text_color(p.tx3).child(format!(
                "已核实内容：左 {} KiB · 右 {} KiB",
                bytes["left"].as_u64().unwrap_or(0) / 1024,
                bytes["right"].as_u64().unwrap_or(0) / 1024
            )));
        }
        if let Some(summary) = value["summary"].as_object() {
            card = card.child(
                div().text_color(p.tx2).whitespace_normal().child(
                    summary
                        .iter()
                        .map(|(k, v)| format!("{k} {v}"))
                        .collect::<Vec<_>>()
                        .join(" · "),
                ),
            );
        }
        let filter_key = key.to_owned();
        let only_changes = self.comparison_only_changes.contains(key);
        card = card.child(
            agent_ui::step_action(
                SharedString::from(format!("{key}:filter")),
                if only_changes {
                    "显示全部"
                } else {
                    "只看差异"
                },
                cx,
            )
            .on_click(cx.listener(move |s, _, _, cx| {
                if !s.comparison_only_changes.remove(&filter_key) {
                    s.comparison_only_changes.insert(filter_key.clone());
                }
                cx.notify();
            })),
        );
        for (index, row) in rows.iter().enumerate() {
            if only_changes
                && matches!(
                    row["status"].as_str(),
                    Some("内容相同" | "元数据相同" | "链接相同")
                )
            {
                continue;
            }
            let text = format!(
                "{}\n{}",
                row["path"].as_str().unwrap_or_default(),
                row["status"].as_str().unwrap_or_default()
            );
            card = card.child(
                super::subagent_ui::selectable(format!("{key}:diff:{index}"), &text, w, cx)
                    .py(px(5.))
                    .border_b_1()
                    .border_color(p.line)
                    .whitespace_normal(),
            );
            if row["left"]["kind"] == "file" && row["right"]["kind"] == "file" {
                let preview_key = format!("{key}:preview:{index}");
                let click_key = preview_key.clone();
                let cursor = value["cursor"].as_str().unwrap_or_default().to_owned();
                let path = row["path"].as_str().unwrap_or_default().to_owned();
                card = card.child(
                    agent_ui::step_action(
                        SharedString::from(format!("{preview_key}:open")),
                        "查看文本差异",
                        cx,
                    )
                    .on_click(cx.listener(move |s, _, w, cx| {
                        s.open_comparison_preview(
                            click_key.clone(),
                            cursor.clone(),
                            path.clone(),
                            w,
                            cx,
                        )
                    })),
                );
                if let Some(text) = self.comparison_previews.get(&preview_key) {
                    card = card.child(
                        super::subagent_ui::selectable(preview_key, text, w, cx)
                            .whitespace_normal()
                            .text_size(px(11.)),
                    );
                }
            }
        }
        if value["next_offset"].is_number() {
            card = card.child(
                div()
                    .text_color(p.tx3)
                    .child("完整结果可展开输出查看，或让 AI 继续读取下一页"),
            );
        }
        Some(card)
    }
}
