use super::*;
use shellmate_core::Newline;
impl HostEditor {
    pub(super) fn change_protocol(
        &mut self,
        protocol: Protocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.protocol;
        self.protocol = protocol;
        self.invalidate_test();
        self.error = None;
        if protocol == Protocol::Telnet {
            self.auth = AuthMethod::Password;
            if previous != protocol {
                self.set_text(&self.port, "23", window, cx);
            }
        }
        if matches!(protocol,Protocol::Ssh|Protocol::Mosh) && !matches!(previous,Protocol::Ssh|Protocol::Mosh) {
            self.set_text(&self.port, "22", window, cx);
        }
        if protocol == Protocol::Serial {
            self.scan_devices(window, cx);
        }
        cx.notify();
    }
    pub(super) fn scan_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if crate::prototype_fixture() {
            self.devices = vec![shellmate_serial::Device {
                path: "/dev/cu.usbserial-0001".into(),
                description: "Silicon Labs CP2102 · ESP32".into(),
            }];
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let devices = cx
                .background_executor()
                .spawn(async { shellmate_serial::devices() })
                .await;
            this.update(cx, |this, cx| {
                match devices {
                    Ok(devices) => this.devices = devices,
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    pub(super) fn test_stream(&mut self, host: Host, window: &mut Window, cx: &mut Context<Self>) {
        let Some(runtime) = crate::runtime::get(cx) else {
            return;
        };
        let entered = self.credential(&host, cx).map(|c| c.answer.secret);
        let secrets = self.workspace.update(cx, |w, _| w.editor_secrets()).ok();
        let join = runtime.spawn(async move {
            if host.protocol == Protocol::Serial {
                tokio::task::spawn_blocking(move || {
                    shellmate_serial::test_connection(&host.addr, &host.serial)
                })
                .await
                .map_err(|_| "串口测试已中止".to_string())??;
                return Ok("连接成功 · 串口已关闭".to_string());
            }
            let password = match entered {
                Some(p) => Some(p),
                None => match secrets {
                    Some(s) => s
                        .get(&shellmate_store::SecretKey::HostPassword(host.id.clone()))
                        .map_err(|e| e.to_string())?,
                    None => None,
                },
            };
            shellmate_telnet::test_connection(
                &host.addr,
                host.port,
                host.user.unwrap_or_default(),
                password,
            )
            .await
        });
        self.test_abort = Some(join.abort_handle());
        self.test_tasks
            .push(cx.spawn_in(window, async move |this, cx| {
                let result = join.await;
                this.update(cx, |this, cx| {
                    this.test_abort = None;
                    this.test_result = Some(match result {
                        Ok(Ok(s)) => (true, s),
                        Ok(Err(e)) => (false, e),
                        Err(_) => (false, "连接测试已中止".into()),
                    });
                    cx.notify();
                })
                .ok();
            }));
        cx.notify();
    }
    fn serial_choice(
        &self,
        key: &'static str,
        label: &'static str,
        value: String,
        options: &[&str],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let weak = cx.entity().downgrade();
        let options = options.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let button = Button::new(key)
            .ghost()
            .h(px(38.))
            .w_full()
            .justify_start()
            .px(px(12.))
            .rounded(px(11.))
            .bg(p.card2)
            .text_color(p.tx)
            .text_size(px(13.))
            .font_weight(FontWeight::NORMAL)
            .accessibility_label(value.clone())
            // Button's built-in label lives in a centered inner flex container.
            // Fill that container so the value starts at the field's 12 px inset.
            .child(
                div()
                    .w_full()
                    .text_left()
                    .text_size(px(13.))
                    .line_height(px(16.))
                    .child(value),
            )
            .dropdown_menu(move |mut menu, _, _| {
                for value in &options {
                    let value = value.clone();
                    let weak = weak.clone();
                    menu =
                        menu.item(PopupMenuItem::new(value.clone()).on_click(move |_, _, cx| {
                            weak.update(cx, |this, cx| {
                                match key {
                                    "serial-bits" => {
                                        this.serial.data_bits = value.parse().unwrap_or(8)
                                    }
                                    "serial-parity" => {
                                        this.serial.parity = match value.as_str() {
                                            "偶" => 'E',
                                            "奇" => 'O',
                                            _ => 'N',
                                        }
                                    }
                                    "serial-stop" => {
                                        this.serial.stop_bits = value.parse().unwrap_or(1)
                                    }
                                    "serial-newline" => {
                                        this.serial.newline = match value.as_str() {
                                            "LF" => Newline::Lf,
                                            "CR" => Newline::Cr,
                                            _ => Newline::CrLf,
                                        }
                                    }
                                    "serial-flow" => {
                                        this.serial.flow = match value.as_str() {
                                            "硬件 RTS/CTS" => "hardware",
                                            "软件 XON/XOFF" => "software",
                                            _ => "none",
                                        }
                                        .into()
                                    }
                                    _ => {}
                                }
                                this.invalidate_test();
                                cx.notify();
                            })
                            .ok();
                        }));
                }
                menu
            });
        div()
            .flex_1()
            .min_w_0()
            .child(self.field(label, p, button))
            .into_any_element()
    }
    pub(super) fn render_serial_fields(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let weak = cx.entity().downgrade();
        let devices = self.devices.clone();
        let selected = self
            .devices
            .iter()
            .find(|d| d.path == self.addr.read(cx).value().as_str())
            .map_or("", |d| d.description.as_str());
        let picker = Button::new("serial-devices")
            .ghost()
            .compact()
            .label(format!("{selected}  ▾"))
            .text_size(px(12.))
            .text_color(p.tx2)
            .max_w(px(260.))
            .dropdown_menu(move |mut menu, _, _| {
                for device in &devices {
                    let path = device.path.clone();
                    let weak = weak.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!("{} · {}", device.path, device.description))
                            .on_click(move |_, w, cx| {
                                weak.update(cx, |this, cx| {
                                    this.set_text(&this.addr, &path, w, cx);
                                    this.invalidate_test();
                                })
                                .ok();
                            }),
                    );
                }
                let weak = weak.clone();
                menu.item(
                    PopupMenuItem::new("刷新设备列表").on_click(move |_, w, cx| {
                        weak.update(cx, |this, cx| this.scan_devices(w, cx)).ok();
                    }),
                )
            });
        let weak = cx.entity().downgrade();
        let bauds = [9600, 115200, 460800, 921600];
        let current = self.baud.read(cx).value().parse::<u32>().unwrap_or(0);
        let preset = segmented(
            "serial-baud-presets",
            bauds
                .iter()
                .map(|b| SegmentItem::new(b.to_string(), *b == current))
                .collect(),
            move |i, w, cx| {
                weak.update(cx, |this, cx| {
                    this.set_text(&this.baud, &bauds[i].to_string(), w, cx);
                    this.invalidate_test();
                })
                .ok();
            },
            cx,
        )
        .font_family(crate::theme::MONO_FONT)
        .h(px(36.));
        let label = div()
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(12.))
            .line_height(px(17.))
            .text_color(p.tx2)
            .child("波特率")
            .child(
                div()
                    .id("serial-advanced")
                    .cursor_pointer()
                    .text_color(p.acc)
                    .child(if self.advanced {
                        "收起高级选项"
                    } else {
                        "自定义 / 高级…"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.advanced = !this.advanced;
                        cx.notify();
                    })),
            );
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(self.field(
                "串口设备",
                p,
                self.input(&self.addr, p, true).h(px(40.)).suffix(picker),
            ))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(label)
                    .child(preset),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(px(10.))
                    .child(self.serial_choice(
                        "serial-bits",
                        "数据位",
                        self.serial.data_bits.to_string(),
                        &["5", "6", "7", "8"],
                        p,
                        cx,
                    ))
                    .child(
                        self.serial_choice(
                            "serial-parity",
                            "校验",
                            match self.serial.parity {
                                'E' => "偶",
                                'O' => "奇",
                                _ => "无",
                            }
                            .into(),
                            &["无", "偶", "奇"],
                            p,
                            cx,
                        ),
                    )
                    .child(self.serial_choice(
                        "serial-stop",
                        "停止位",
                        self.serial.stop_bits.to_string(),
                        &["1", "2"],
                        p,
                        cx,
                    ))
                    .child(self.serial_choice(
                        "serial-newline",
                        "发送换行",
                        self.serial.newline.label().into(),
                        &["CRLF", "LF", "CR"],
                        p,
                        cx,
                    )),
            )
            .when(self.advanced, |d| {
                d.child(
                    div()
                        .flex()
                        .gap(px(14.))
                        .child(div().flex_1().min_w_0().child(self.field(
                            "名称（留空使用设备名）",
                            p,
                            self.input(&self.name, p, false),
                        )))
                        .child(div().flex_1().min_w_0().child(self.field(
                            "分组",
                            p,
                            self.input(&self.group, p, false),
                        ))),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(14.))
                        .child(div().flex_1().min_w_0().child(self.field(
                            "自定义波特率",
                            p,
                            self.input(&self.baud, p, true),
                        )))
                        .child(
                            self.serial_choice(
                                "serial-flow",
                                "流控",
                                match self.serial.flow.as_str() {
                                    "hardware" => "硬件 RTS/CTS",
                                    "software" => "软件 XON/XOFF",
                                    _ => "无",
                                }
                                .into(),
                                &["无", "硬件 RTS/CTS", "软件 XON/XOFF"],
                                p,
                                cx,
                            ),
                        ),
                )
            })
            .into_any_element()
    }
}
