//! Inert terminal menu for native control/takeover review; no shell or network.
use gpui::{App, Entity};
use shellmate_core::terminal_interaction::{Action, Controller, Screen};
use shellmate_term::{OutputSink, Terminal, TerminalBackend};
use std::sync::Arc;
struct Menu {
    sink: OutputSink,
    selected: usize,
}
impl Menu {
    fn render(&self) {
        let content = format!(
            "\x1b[?1049h\x1b[?1h\x1b[H\x1b[2JTerminal menu\r\n\r\n{} Inspect logs\x1b[0m\r\n{} Inspect configuration\x1b[0m\r\n\r\nUse arrows to choose. Enter opens selection.\x1b[8;1H",
            if self.selected == 0 {
                "\x1b[7m >"
            } else {
                "  "
            },
            if self.selected == 1 {
                "\x1b[7m >"
            } else {
                "  "
            }
        );
        self.sink.output(content.as_bytes());
    }
}
impl TerminalBackend for Menu {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.ends_with(b"B") {
            self.selected = 1;
        }
        if bytes.ends_with(b"A") {
            self.selected = 0;
        }
        let copy = Self {
            sink: self.sink.clone(),
            selected: self.selected,
        };
        std::thread::spawn(move || copy.render());
        Ok(())
    }
    fn resize(&mut self, _: u16, _: u16, _: u16, _: u16) -> std::io::Result<()> {
        Ok(())
    }
    fn close(&mut self) {}
}
pub fn install(terminal: &Entity<Terminal>, cx: &mut App) -> Screen {
    terminal.update(cx, |t, cx| {
        t.attach(
            |sink, _| {
                let menu = Menu { sink, selected: 0 };
                menu.render();
                Ok(Box::new(menu))
            },
            cx,
        );
        let screen = t.interaction_snapshot(None).unwrap();
        let owner = Arc::new(Controller::new("检查菜单".into()));
        t.interaction_input(
            &screen.screen_id,
            &[Action::Key {
                key: "down".into(),
                repeat: 1,
            }],
            owner,
            cx,
        )
        .unwrap()
        .screen
    })
}
