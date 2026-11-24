use super::*;
impl AiSettings {
    pub(super) fn memory_card(&self, cx: &mut Context<Self>) -> Div {
        let p = cx.palette().clone();
        let enabled = self
            .db
            .borrow()
            .ai_settings()
            .unwrap_or_default()
            .auto_memory;
        settings_card(cx)
            .child(
                div()
                    .pt(px(14.))
                    .pb(px(4.))
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("记忆"),
            )
            .child(settings_row(
                "自动记忆",
                "AI 会把偏好、纠正和主机背景记成笔记，下次对话自动参考",
                crate::ui::Toggle::new("ai-auto-memory")
                    .checked(enabled)
                    .color(p.ok)
                    .on_click(cx.listener(|s, on: &bool, _, cx| {
                        match s.db.borrow_mut().set_auto_memory(*on) {
                            Ok(()) => {}
                            Err(e) => s.result = Some(e.to_string()),
                        };
                        s.notify_configuration(cx);
                        cx.notify();
                    })),
                cx,
            ))
            .child(settings_row(
                "全局说明",
                "对所有主机生效的 AI 说明（AGENTS.md）",
                Button::new("ai-memory-global")
                    .label("编辑")
                    .on_click(cx.listener(|s, _, w, cx| {
                        s.open_memory(
                            Some(crate::ai::memory_model::Focus::global_instructions()),
                            w,
                            cx,
                        )
                    })),
                cx,
            ))
            .child(settings_row(
                "记忆管理",
                "查看和编辑 AI 说明、主题笔记与远端信任",
                Button::new("ai-memory-manage")
                    .label("打开")
                    .on_click(cx.listener(|s, _, w, cx| s.open_memory(None, w, cx))),
                cx,
            ))
            .child(settings_row(
                "在 Finder 中打开",
                "查看本机保存的记忆文件",
                Button::new("ai-memory-folder")
                    .label("打开")
                    .on_click(cx.listener(|s, _, _, cx| {
                        let result = s
                            .workspace
                            .update(cx, |w, _| w.memory_root())
                            .map_err(|e| e.to_string())
                            .and_then(|root| {
                                shellmate_core::memory::Memory::new(root, None)
                                    .directory()
                                    .map_err(|e| e.to_string())
                            });
                        match result {
                            Ok(path) => cx.open_with_system(&path),
                            Err(e) => {
                                s.result = Some(e);
                                cx.notify();
                            }
                        }
                    })),
                cx,
            ))
            .child(
                div().py(px(10.)).flex().justify_end().child(
                    crate::dialogs::button(
                        "ai-memory-clear",
                        "清除所有记忆",
                        crate::dialogs::Tone::Plain,
                        true,
                    )
                    .text_color(p.err)
                    .on_click(cx.listener(|s, _, w, cx| s.clear_memory(w, cx))),
                ),
            )
    }
    fn open_memory(
        &mut self,
        focus: Option<crate::ai::memory_model::Focus>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.workspace.update(cx, |workspace, cx| {
            workspace.open_memory_manager(focus, w, cx)
        }) {
            Ok(Ok(manager)) => {
                cx.observe(&manager, |s, _, cx| {
                    s.notify_configuration(cx);
                    cx.notify();
                })
                .detach();
            }
            Ok(Err(e)) => self.result = Some(e),
            Err(e) => self.result = Some(e.to_string()),
        }
        cx.notify();
    }
    fn clear_memory(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Ok(root) = self.workspace.update(cx, |w, _| w.memory_root()) else {
            return;
        };
        let db = self.db.clone();
        let this = cx.weak_entity();
        crate::dialogs::confirm::ConfirmDialog::open(
            "清除所有记忆？",
            "将删除所有全局、主机说明和记忆，并清除远端说明的信任选择。对话记录会保留。此操作无法撤销。",
            "清除所有记忆",
            true,
            move |_, cx| {
                crate::ai::memory_model::clear_all(&root, &mut db.borrow_mut())?;
                this.update(cx, |s, cx| {
                    s.result = Some("所有记忆及远端信任选择已清除".into());
                    cx.notify();
                })
                .ok();
                Ok(())
            },
            w,
            cx,
        );
    }
}
