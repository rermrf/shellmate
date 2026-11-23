//! Terminal font size: the default in the settings, and each window's temporary zoom (⌘= ⌘− ⌘0).
use super::*;
use crate::theme::DefaultFontSize;
use shellmate_term::TerminalStyle;

/// A window's zoom level changed for good: the settings of that window show the new default.
pub(crate) struct TerminalFontSizeChanged(pub u8);
impl gpui::EventEmitter<TerminalFontSizeChanged> for Workspace {}

/// The zoom of a window after one step of `delta` points. `None` is the default size, which is also
/// what a window returns to when it is zoomed back to it, so that it follows the setting again.
/// The result stays inside the supported range; at its edge a step changes nothing.
fn stepped(current: Option<f32>, default: f32, delta: f32) -> Option<f32> {
    let next = (current.unwrap_or(default) + delta).clamp(DefaultFontSize::MIN, DefaultFontSize::MAX);
    (next != default).then_some(next)
}

impl Workspace {
    /// The style of this window's terminals: the app's colors and font at this window's size.
    pub(super) fn terminal_style(&self, cx: &App) -> TerminalStyle {
        let mut style = cx.terminal_style();
        if let Some(points) = self.font_size {
            style.font_size = px(points);
        }
        style
    }

    /// Gives every terminal of this window the window's current style. The grid follows on the
    /// next paint, which also tells the PTY or the remote end the new size.
    pub(super) fn apply_terminal_style(&mut self, cx: &mut Context<Self>) {
        let style = self.terminal_style(cx);
        for session in &self.sessions {
            session.view.update(cx, |view, cx| view.set_style(style.clone(), cx));
        }
    }

    pub(super) fn zoom_in(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(1., cx);
    }

    pub(super) fn zoom_out(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(-1., cx);
    }

    pub(super) fn zoom_reset(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(None, cx);
    }

    fn zoom_by(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_zoom(stepped(self.font_size, DefaultFontSize::get(cx), delta), cx);
    }

    fn set_zoom(&mut self, font_size: Option<f32>, cx: &mut Context<Self>) {
        if self.font_size == font_size {
            return;
        }
        self.font_size = font_size;
        self.apply_terminal_style(cx);
        cx.notify();
    }

    /// Saves the default font size (settings) and gives it to every window that is not zoomed.
    pub(crate) fn set_terminal_font_size(&mut self, points: u8, cx: &mut Context<Self>) -> Result<(), String> {
        let points = self.hub.store.borrow_mut().set_terminal_font_size(points).map_err(|e| e.to_string())?;
        self.hub.preferences.borrow_mut().terminal_font_size = points;
        let default = f32::from(points);
        cx.set_global(DefaultFontSize(default));
        self.project(Route::All, cx, move |workspace, cx| {
            // A window zoomed to the new default is no longer zoomed.
            if workspace.font_size == Some(default) {
                workspace.font_size = None;
            }
            workspace.apply_terminal_style(cx);
            cx.emit(TerminalFontSizeChanged(points));
            cx.notify();
        });
        Ok(())
    }

    /// Debug review hook: the font size and every terminal's grid of this window.
    #[cfg(debug_assertions)]
    fn zoom_report(&self, cx: &App) -> String {
        let grids: Vec<String> = self.sessions.iter().map(|session| {
            let size = session.terminal.read(cx).size();
            format!("{} {}x{}", session.title, size.cols, size.rows)
        }).collect();
        let points = self.font_size.unwrap_or_else(|| DefaultFontSize::get(cx));
        format!("font {points}pt (default {}pt, {}) terminals [{}]", DefaultFontSize::get(cx),
            if self.font_size.is_some() { "zoomed" } else { "not zoomed" }, grids.join(", "))
    }
}

/// `SHELLMATE_DEBUG_ZOOM_AFTER=<seconds>:<steps>`: after that long the production zoom actions run
/// in the front window, `steps` times (negative: zoom out). The log shows the font size and every
/// terminal's columns and rows before, right after and, once the windows have painted, settled.
#[cfg(debug_assertions)]
pub(super) fn debug_hook(cx: &mut App) {
    use std::time::Duration;
    let Ok(spec) = std::env::var("SHELLMATE_DEBUG_ZOOM_AFTER") else { return };
    let Some((seconds, steps)) = parse_debug_spec(&spec) else {
        log::warn!("invalid SHELLMATE_DEBUG_ZOOM_AFTER {spec:?}: expected <seconds>:<steps>, e.g. 5:+2 or 5:-3");
        return;
    };
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(seconds)).await;
        cx.update(|cx| Hub::update(cx, move |workspace, window, cx| {
            log::info!("zoom before: {}", workspace.zoom_report(cx));
            for _ in 0..steps.unsigned_abs() {
                if steps > 0 { workspace.zoom_in(window, cx) } else { workspace.zoom_out(window, cx) }
            }
            log::info!("zoom applied ({steps:+} steps): {}", workspace.zoom_report(cx));
        }));
        // The grid is measured when the window paints; give it time.
        cx.background_executor().timer(Duration::from_millis(1500)).await;
        cx.update(|cx| {
            for entry in Hub::windows(cx).to_vec() {
                if let Some(workspace) = entry.workspace.upgrade() {
                    log::info!("zoom settled: window {} {}", entry.number, workspace.read(cx).zoom_report(cx));
                }
            }
        });
    })
    .detach();
}

#[cfg(debug_assertions)]
fn parse_debug_spec(spec: &str) -> Option<(u64, i32)> {
    let (seconds, steps) = spec.split_once(':')?;
    Some((seconds.trim().parse().ok()?, steps.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: f32 = 13.;

    /// Presses a key `times` times the way the window does.
    fn press(start: Option<f32>, delta: f32, times: usize) -> Option<f32> {
        (0..times).fold(start, |current, _| stepped(current, DEFAULT, delta))
    }

    #[test]
    fn each_step_is_one_point_and_the_default_is_no_zoom() {
        assert_eq!(stepped(None, DEFAULT, 1.), Some(14.));
        assert_eq!(stepped(None, DEFAULT, -1.), Some(12.));
        assert_eq!(stepped(Some(14.), DEFAULT, 1.), Some(15.));
        assert_eq!(stepped(Some(12.), DEFAULT, -1.), Some(11.));
        // Back at the default size the window follows the setting again.
        assert_eq!(stepped(Some(14.), DEFAULT, -1.), None);
        assert_eq!(stepped(Some(12.), DEFAULT, 1.), None);
        assert_eq!(press(None, 1., 3), Some(16.));
        assert_eq!(press(Some(16.), -1., 3), None);
    }

    #[test]
    fn zoom_stops_at_nine_and_twenty_eight_without_error() {
        assert_eq!(press(None, -1., 4), Some(9.));
        assert_eq!(press(None, -1., 40), Some(9.), "more presses change nothing");
        assert_eq!(press(None, 1., 15), Some(28.));
        assert_eq!(press(None, 1., 40), Some(28.));
        assert_eq!(stepped(Some(9.), DEFAULT, 1.), Some(10.), "and it leaves the edge again");
        assert_eq!(stepped(Some(28.), DEFAULT, -1.), Some(27.));
    }

    #[test]
    fn zoom_at_an_extreme_default_stays_inside_the_range() {
        assert_eq!(stepped(None, 28., 1.), None);
        assert_eq!(stepped(None, 28., -1.), Some(27.));
        assert_eq!(stepped(None, 9., -1.), None);
        assert_eq!(stepped(None, 9., 1.), Some(10.));
    }

    #[test]
    fn the_range_is_the_one_the_settings_save() {
        assert_eq!((DefaultFontSize::MIN, DefaultFontSize::MAX, DefaultFontSize::DEFAULT), (9., 28., DEFAULT));
        assert_eq!(shellmate_store::TerminalFontSize::clamp(255) as f32, DefaultFontSize::MAX);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn the_debug_hook_reads_seconds_and_signed_steps() {
        assert_eq!(parse_debug_spec("5:+2"), Some((5, 2)));
        assert_eq!(parse_debug_spec("5:2"), Some((5, 2)));
        assert_eq!(parse_debug_spec("0:-3"), Some((0, -3)));
        assert_eq!(parse_debug_spec(" 7 : -1 "), Some((7, -1)));
        for bad in ["", "5", "x:1", "5:", "-5:1", "5:1.5", "5:1:2"] {
            assert_eq!(parse_debug_spec(bad), None, "{bad:?}");
        }
    }
}
