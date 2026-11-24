//! Blocking project-document trust card in the existing drawer approval surface.
use super::{Drawer, agent_ui};
use crate::{ai::remote::TrustBatch, theme::ActivePalette};
use gpui::{
    component::{
        Disableable,
        button::{Button, ButtonVariants},
    },
    prelude::*,
    *,
};
use shellmate_core::remote_instructions::Discovery;
use tokio::sync::oneshot;
pub(super) struct PendingTrust {
    pub id: String,
    pub batch: TrustBatch,
    pub expanded: std::collections::HashSet<usize>,
    pub key: String,
    pub persist: bool,
    pub reply: Option<oneshot::Sender<Discovery>>,
}
impl Drawer {
    pub(super) fn ask_remote_trust(
        &mut self,
        id: String,
        discovery: Discovery,
        reply: oneshot::Sender<Discovery>,
    ) {
        if reply.is_closed() {
            return;
        }
        let key = self.permission_key().to_owned();
        let persist = self.memory_files().has_host();
        let batch =
            self.state
                .remote_decisions
                .prepare(&self.db.borrow(), &key, persist, discovery);
        if batch.unknown.is_empty() {
            let _ = reply.send(batch.finish(false));
            return;
        }
        self.state.remote_trust = Some(PendingTrust {
            id,
            batch,
            expanded: Default::default(),
            key,
            persist,
            reply: Some(reply),
        });
        self.state.follow_bottom = true;
    }
    fn finish_remote_trust(&mut self, cancel: bool, cx: &mut Context<Self>) {
        let Some(pending) = self.state.remote_trust.as_ref() else {
            return;
        };
        if !cancel {
            if pending
                .batch
                .unknown
                .iter()
                .any(|i| pending.batch.choices[*i].is_none())
            {
                return;
            }
            if !self.fixture
                && self
                    .state
                    .remote_decisions
                    .save(
                        &mut self.db.borrow_mut(),
                        &pending.key,
                        pending.persist,
                        &pending.batch,
                    )
                    .is_err()
            {
                self.state.save_error = Some("无法保存远端说明信任设置，请重试".into());
                cx.notify();
                return;
            }
        }
        let pending = self.state.remote_trust.take().unwrap();
        if let Some(reply) = pending.reply {
            let _ = reply.send(pending.batch.finish(cancel));
        }
        self.state.save_error = None;
        cx.notify();
    }
    pub(super) fn remote_trust_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let pending = self.state.remote_trust.as_ref().unwrap();
        let single = pending.batch.unknown.len() == 1;
        let complete = pending
            .batch
            .unknown
            .iter()
            .all(|i| pending.batch.choices[*i].is_some());
        let mut card = agent_ui::step_surface(p)
            .id("ai-remote-trust-card")
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(8.))
            .text_size(px(12.))
            .text_color(p.tx);
        for (position, &index) in pending.batch.unknown.iter().enumerate() {
            if position > 0 {
                // The card's 8 px gap plus 4 px margin gives 12 px on each side.
                card = card.child(div().flex_none().h(px(1.)).my(px(4.)).bg(p.line));
            }
            let doc = &pending.batch.discovery.files[index];
            let expanded = pending.expanded.contains(&index);
            let choice = pending.batch.choices[index];
            let title = format!("此主机的 {} 将作为 AI 说明加载", doc.path);
            let size = format!(
                "{} KB{}",
                (doc.bytes as f64 / 1024. * 10.).round() / 10.,
                if doc.truncated { " · 已截断" } else { "" }
            );
            let mut row = div()
                .flex()
                .flex_col()
                .gap(px(7.))
                .child(div().whitespace_normal().child(title))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_none()
                                .text_color(p.tx3)
                                .text_size(px(11.))
                                .child(size),
                        )
                        .child(
                            agent_ui::step_action(
                                SharedString::from(format!("remote-preview-{index}")),
                                if expanded {
                                    "收起预览"
                                } else {
                                    "预览前几行"
                                },
                                cx,
                            )
                            .on_click(cx.listener(
                                move |s, _, _, cx| {
                                    if let Some(p) = &mut s.state.remote_trust
                                        && !p.expanded.remove(&index)
                                    {
                                        p.expanded.insert(index);
                                    }
                                    cx.notify();
                                },
                            )),
                        ),
                );
            if expanded {
                let preview: String = doc
                    .text
                    .lines()
                    .take(8)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .chars()
                    .take(1200)
                    .collect();
                row = row.child(agent_ui::command_surface(cx).child(preview));
            }
            let actions = div()
                .flex()
                .gap(px(4.))
                .child(
                    Button::new(SharedString::from(format!("remote-trust-{index}")))
                        .label("信任")
                        .when(choice == Some(true), |button| button.primary())
                        .h(px(26.))
                        .on_click(cx.listener(move |s, _, _, cx| {
                            if let Some(p) = &mut s.state.remote_trust {
                                p.batch.choices[index] = Some(true);
                            }
                            if single {
                                s.finish_remote_trust(false, cx);
                            } else {
                                cx.notify();
                            }
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("remote-ignore-{index}")))
                        .label("忽略")
                        .when(choice == Some(false), |button| button.primary())
                        .h(px(26.))
                        .on_click(cx.listener(move |s, _, _, cx| {
                            if let Some(p) = &mut s.state.remote_trust {
                                p.batch.choices[index] = Some(false);
                            }
                            if single {
                                s.finish_remote_trust(false, cx);
                            } else {
                                cx.notify();
                            }
                        })),
                );
            card = card.child(row.child(actions));
        }
        let mut actions = div().flex().gap(px(6.));
        if !single {
            actions = actions.child(
                Button::new("remote-confirm")
                    .label("确定")
                    .primary()
                    .h(px(27.))
                    .disabled(!complete)
                    .on_click(cx.listener(|s, _, _, cx| s.finish_remote_trust(false, cx))),
            );
        }
        actions = actions.child(
            Button::new("remote-cancel")
                .label("本次忽略")
                .h(px(27.))
                .on_click(cx.listener(|s, _, _, cx| s.finish_remote_trust(true, cx))),
        );
        card.child(actions)
    }
}
