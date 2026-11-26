use super::*;

impl Render for OutputPane {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette().clone();
        let expanded = self.expanded;
        let mut body = div().min_w_0().w_full().flex().flex_col().gap(px(8.));
        if self.expanded && self.source.running && !self.dragging {
            let handle = self.viewport.read(cx).handle.clone();
            if handle.max_offset().y > px(0.)
                && f32::from(handle.offset().y + handle.max_offset().y).abs() < 2.
            {
                self.follow = true;
            }
        }
        if let Some(doc) = self.doc.clone() {
            let total_rows = doc.rows;
            let rows = if expanded {
                total_rows
            } else {
                total_rows.min(8)
            };
            let handle = self.viewport.read(cx).handle.clone();
            let font = font(MONO_FONT);
            let cell = f32::from(
                w.text_system()
                    .shape_line(
                        "M".into(),
                        px(12.5),
                        &[TextRun {
                            len: 1,
                            font: font.clone(),
                            color: p.tx2,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    )
                    .width(),
            );
            let geometry = self.geometry.clone();
            let paint_geometry = self.geometry.clone();
            let visible = self.last_visible.clone();
            let entity = cx.entity().downgrade();
            let paint_entity = entity.clone();
            let pages = self.pages.clone();
            let revision = self.revision;
            let selection = self.selection.map(|(a, b)| a.min(b)..a.max(b));
            let accs = p.accs;
            let color = p.tx2;
            let follow = self.follow && self.source.running;
            let canvas = canvas(
                move |bounds, w, cx| {
                    let clip = bounds.intersect(&w.content_mask().bounds);
                    *geometry.borrow_mut() = Geometry {
                        origin: bounds.origin,
                        viewport: clip,
                        hits: Vec::new(),
                    };
                    if clip.size.width > px(0.) && clip.size.height > px(0.) {
                        *visible.borrow_mut() = Instant::now();
                    }
                    let first = (f32::from(clip.top() - bounds.top()).max(0.) / 18.) as usize;
                    let last = ((f32::from(clip.bottom() - bounds.top()).max(0.) / 18.).ceil()
                        as usize
                        + 1)
                    .min(rows);
                    let col = (f32::from(clip.left() - bounds.left()).max(0.) / cell.max(1.))
                        as usize
                        / 128
                        * 128;
                    let cols = ((f32::from(clip.size.width) / cell.max(1.)).ceil() as usize + 256)
                        .div_ceil(128)
                        * 128;
                    let key = (
                        expanded,
                        first / 32 * 32,
                        (last.div_ceil(32) * 32).min(rows),
                        col,
                        cols,
                    );
                    let weak = entity.clone();
                    if !pages.contains_key(&key) {
                        cx.defer(move |cx| {
                            weak.update(cx, |s, cx| {
                                if s.revision == revision {
                                    s.request_page(key, cx);
                                }
                            })
                            .ok();
                        });
                    }
                    if follow {
                        let max = handle.max_offset();
                        if max.y > px(0.) {
                            handle.set_offset(point(handle.offset().x, -max.y));
                        }
                    }
                    (first, last, key, pages.get(&key).cloned())
                },
                move |bounds, (first, last, _, page), w, cx| {
                    if let Some(page) = page {
                        for segment in page.iter() {
                            let row =
                                if !expanded && total_rows > 8 && segment.row >= total_rows - 4 {
                                    segment.row - (total_rows - 8)
                                } else {
                                    segment.row
                                };
                            if row < first || row >= last {
                                continue;
                            }
                            let origin = bounds.origin
                                + point(px(segment.column as f32 * cell), px(row as f32 * 18.));
                            let run = TextRun {
                                len: segment.text.len(),
                                font: font.clone(),
                                color,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            };
                            let line = w.text_system().shape_line(
                                segment.text.clone().into(),
                                px(12.5),
                                &[run],
                                None,
                            );
                            if let Some(range) = selection
                                .as_ref()
                                .and_then(|range| segment.selected_range(range))
                            {
                                let start = line.x_for_index(range.start);
                                let end = line.x_for_index(range.end);
                                w.paint_quad(fill(
                                    Bounds::new(
                                        origin + point(start, px(0.)),
                                        size((end - start).max(px(0.)), px(18.)),
                                    ),
                                    accs,
                                ));
                            }
                            paint_geometry.borrow_mut().hits.push((
                                segment.row,
                                segment
                                    .carets
                                    .iter()
                                    .map(|(index, byte)| {
                                        (origin.x + line.x_for_index(*index), *byte)
                                    })
                                    .collect(),
                            ));
                            line.paint(origin, px(18.), TextAlign::Left, None, w, cx)
                                .ok();
                        }
                    }
                    if !expanded && total_rows > 8 {
                        let text = format!("… 省略 {} 行", total_rows - 7);
                        let run = TextRun {
                            len: text.len(),
                            font: font.clone(),
                            color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        w.text_system()
                            .shape_line(text.into(), px(12.5), &[run], None)
                            .paint(
                                bounds.origin + point(px(0.), px(3. * 18.)),
                                px(18.),
                                TextAlign::Left,
                                None,
                                w,
                                cx,
                            )
                            .ok();
                    }
                    w.on_mouse_event(move |event: &MouseMoveEvent, phase, w, cx| {
                        if phase.bubble() {
                            paint_entity
                                .update(cx, |s, cx| {
                                    if s.dragging {
                                        s.pointer = event.position;
                                        s.select_at(event.position, cx);
                                        w.refresh();
                                    }
                                })
                                .ok();
                        }
                    });
                },
            )
            .w(px((doc.columns as f32 * cell + 8.).max(1.)))
            .h(px((rows as f32 * 18.).max(18.)));
            body = body.child(
                div()
                    .key_context("AiOutput")
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::copy_selection))
                    .on_action(cx.listener(Self::select_all))
                    .on_action(
                        cx.listener(|s, _: &Top, _, cx| s.navigate(Some(false), None, 0., cx)),
                    )
                    .on_action(
                        cx.listener(|s, _: &Bottom, _, cx| s.navigate(Some(true), None, 0., cx)),
                    )
                    .on_action(
                        cx.listener(|s, _: &LeftEdge, _, cx| s.navigate(None, Some(false), 0., cx)),
                    )
                    .on_action(
                        cx.listener(|s, _: &RightEdge, _, cx| s.navigate(None, Some(true), 0., cx)),
                    )
                    .on_action(cx.listener(|s, _: &PageUp, _, cx| s.navigate(None, None, 1., cx)))
                    .on_action(
                        cx.listener(|s, _: &PageDown, _, cx| s.navigate(None, None, -1., cx)),
                    )
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|s, _, _, cx| {
                            s.dragging = false;
                            cx.notify();
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|s, _, _, cx| {
                            s.dragging = false;
                            cx.notify();
                        }),
                    )
                    .on_scroll(cx.listener(|s, e: &ScrollEvent, _, cx| {
                        if e.delta.pixel_delta(px(18.)).y > px(0.) {
                            s.follow = false;
                            cx.emit(Reading);
                        }
                        cx.notify();
                    }))
                    .child(
                        ScrollArea::new(
                            "output-viewport",
                            div().flex_none().child(canvas),
                            p.card2,
                        )
                        .retained(self.viewport.clone())
                        .selection(self.selection_scroll.clone())
                        .height_limit(if expanded { 320. } else { 144. }, expanded),
                    ),
            );
        } else {
            let preview = compact(&self.source.preview, self.source.incomplete);
            if preview.is_empty() {
                body = body.child(div().text_size(px(12.5)).text_color(p.tx3).child(
                    if self.source.running {
                        "等待输出…"
                    } else {
                        "（无输出）"
                    },
                ));
            } else {
                body = body.child(agent_ui::output_scroll(
                    "output-preview".into(),
                    div()
                        .flex_none()
                        .child(markdown::plain("output-preview-text", preview)),
                    false,
                    self.selection_scroll.clone(),
                    &p,
                ));
            }
        }
        let note = if self.copy_error {
            Some("复制失败，请重试")
        } else if self.loading && self.doc.is_none() {
            Some("正在读取输出…")
        } else if self.status == ReadStatus::Missing {
            Some("完整输出已清理，仅保留预览")
        } else if self.status == ReadStatus::Failed {
            Some("暂时无法读取完整输出")
        } else if self
            .doc
            .as_ref()
            .is_some_and(|d| d.info.completeness == Completeness::HeadTail)
        {
            Some("输出过大，部分内容未保留")
        } else if self
            .doc
            .as_ref()
            .is_some_and(|d| d.info.completeness == Completeness::Unknown)
        {
            Some("此记录仅能确认已有输出")
        } else {
            None
        };
        if let Some(note) = note {
            body = body.child(div().text_size(px(11.)).text_color(p.tx3).child(note));
        }
        let has_output = self.source.running
            || self.doc.as_ref().map_or(
                !self.source.preview.is_empty() || self.source.incomplete,
                |d| d.bytes > 0,
            );
        if has_output {
            let mut actions = agent_ui::step_actions();
            let more = self.source.incomplete
                || self.source.running
                || self.source.preview.lines().count() > 8
                || self.doc.as_ref().is_some_and(|d| d.rows > 8);
            if !self.source.key.starts_with("tool:") && (expanded || more) {
                actions = actions.child(
                    agent_ui::step_action(
                        "output-expand",
                        if expanded { "收起" } else { "展开输出" },
                        cx,
                    )
                    .on_click(cx.listener(|s, _, _, cx| s.toggle(cx))),
                );
            }
            if self.status == ReadStatus::Failed {
                actions = actions.child(
                    agent_ui::step_action("output-retry", "重试", cx)
                        .on_click(cx.listener(|s, _, _, cx| s.load(cx))),
                );
            }
            if expanded && self.source.running && !self.follow {
                actions = actions.child(
                    agent_ui::step_action("output-bottom", "回到底部", cx).on_click(cx.listener(
                        |s, _, _, cx| {
                            s.follow = true;
                            s.selection = None;
                            s.load(cx);
                            cx.notify();
                        },
                    )),
                );
            }
            if !self.source.key.starts_with("tool:") {
                actions = actions.child(
                    agent_ui::step_action("output-copy", self.copy_label(), cx)
                        .on_click(cx.listener(|s, _, _, cx| s.copy(None, cx))),
                );
            }
            body = body.child(actions);
        }
        let anchor = self.anchor.clone();
        body.relative().child(
            canvas(
                move |bounds, _, _| {
                    *anchor.borrow_mut() = bounds;
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0(),
        )
    }
}
