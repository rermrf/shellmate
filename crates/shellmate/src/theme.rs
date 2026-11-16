//! Design tokens from the prototype (`THEME` / `TERM`), plus syncing them into
//! gpui-component's theme so its widgets match.

use gpui::component::{Theme, ThemeColor, ThemeMode};
use gpui::{App, Global, Hsla, Pixels, Rgba, SharedString, px};
use shellmate_core::GroupTone;
use shellmate_term::{TerminalPalette, TerminalStyle};

pub const UI_FONT: &str = ".SystemUIFont";
pub const MONO_FONT: &str = "JetBrains Mono";
pub const CJK_FALLBACK: &str = "PingFang SC";

/// UI color tokens. Field names follow the prototype's CSS variables.
#[allow(dead_code)] // the full token set is kept even where P0 doesn't use it yet
#[derive(Clone, Debug)]
pub struct Palette {
    /// Window ground (`--win`).
    pub win: Hsla,
    pub desk: Hsla,
    pub shadow_near: Hsla,
    pub shadow_far: Hsla,
    pub shadow_y: f32,
    pub shadow_blur: f32,
    pub card: Hsla,
    pub card2: Hsla,
    pub card3: Hsla,
    pub line: Hsla,
    pub tx: Hsla,
    pub tx2: Hsla,
    pub tx3: Hsla,
    pub acc: Hsla,
    pub accs: Hsla,
    pub ok: Hsla,
    pub oks: Hsla,
    pub warn: Hsla,
    pub warns: Hsla,
    pub err: Hsla,
    pub errs: Hsla,
    /// Drop shadow color for raised cards.
    pub shadow: Hsla,
    /// Backdrop behind modal dialogs (`--scrim`).
    pub scrim: Hsla,
}

fn hex(v: u32) -> Hsla {
    rgba(v, 1.)
}

fn rgba(v: u32, a: f32) -> Hsla {
    Rgba { r: ((v >> 16) & 0xff) as f32 / 255., g: ((v >> 8) & 0xff) as f32 / 255., b: (v & 0xff) as f32 / 255., a }
        .into()
}

impl Palette {
    pub fn light() -> Self {
        Self {
            win: hex(0xEEF0F4),desk:hex(0xD9DDE4),shadow_near:rgba(0x101828,0.05),shadow_far:rgba(0x101828,0.06),shadow_y:4.,shadow_blur:14.,
            card: hex(0xFFFFFF),
            card2: hex(0xF4F5F8),
            card3: hex(0xE6E8ED),
            line: rgba(0x12161E, 0.08),
            tx: hex(0x15171C),
            tx2: hex(0x5C6370),
            tx3: hex(0x6B7280),
            acc: hex(0x2F6BF5),
            accs: rgba(0x2F6BF5, 0.10),
            ok: hex(0x1FA463),
            oks: rgba(0x1FA463, 0.12),
            warn: hex(0xD98200),
            warns: rgba(0xE08A00, 0.13),
            err: hex(0xE5484D),
            errs: rgba(0xE5484D, 0.11),
            shadow: rgba(0x101828, 0.06),
            scrim: rgba(0x141820, 0.32),
        }
    }

    pub fn dark() -> Self {
        Self {
            win: hex(0x111215),desk:hex(0x2A2D33),shadow_near:rgba(0,0.3),shadow_far:rgba(0,0.25),shadow_y:6.,shadow_blur:18.,
            card: hex(0x1C1D21),
            card2: hex(0x25262B),
            card3: hex(0x2C2E34),
            line: rgba(0xFFFFFF, 0.08),
            tx: hex(0xECEDEF),
            tx2: hex(0xA6ABB4),
            tx3: hex(0x7F858F),
            acc: hex(0x5B8CFF),
            accs: rgba(0x5B8CFF, 0.16),
            ok: hex(0x3CC07F),
            oks: rgba(0x3CC07F, 0.15),
            warn: hex(0xF2A93B),
            warns: rgba(0xF2A93B, 0.15),
            err: hex(0xFF6369),
            errs: rgba(0xFF6369, 0.15),
            shadow: rgba(0x000000, 0.25),
            scrim: rgba(0x000000, 0.55),
        }
    }

    /// (background, foreground) for a group's avatar tint.
    pub fn tone(&self, tone: GroupTone) -> (Hsla, Hsla) {
        match tone {
            GroupTone::Accent => (self.accs, self.acc),
            GroupTone::Warn => (self.warns, self.warn),
            GroupTone::Ok => (self.oks, self.ok),
            GroupTone::Neutral => (self.card3, self.tx2),
        }
    }
}

/// Large card corner radius (`--tr`).
pub const CARD_RADIUS: Pixels = px(18.);

pub struct AppTheme {
    pub dark: bool,
    pub palette: Palette,
    pub terminal: TerminalPalette,
}

impl Global for AppTheme {}

pub trait ActivePalette {
    fn palette(&self) -> &Palette;
    fn is_dark(&self) -> bool;
    fn terminal_style(&self) -> TerminalStyle;
}

impl ActivePalette for App {
    fn palette(&self) -> &Palette {
        &self.global::<AppTheme>().palette
    }

    fn is_dark(&self) -> bool {
        self.global::<AppTheme>().dark
    }

    fn terminal_style(&self) -> TerminalStyle {
        TerminalStyle {
            font_family: SharedString::new_static(MONO_FONT),
            fallbacks: vec![CJK_FALLBACK.to_string(), "Menlo".to_string()],
            font_size: px(13.),
            line_height: 20. / 13.,
            palette: self.global::<AppTheme>().terminal.clone(),
        }
    }
}

/// Blends `a` toward `b` by `t` (0 = `a`, 1 = `b`), keeping `a`'s alpha.
fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    Rgba { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: a.a }.into()
}

/// Mirrors the palette into every gpui-component color the app's widgets rely on, so buttons,
/// checkboxes, switches, links, lists, tables and the dialog backdrop follow the prototype
/// (blue `--acc` primary, `--card2` secondary, `--scrim` backdrop).
fn sync_component_colors(c: &mut ThemeColor, p: &Palette, dark: bool) {
    let white = hex(0xFFFFFF);
    // Hover and pressed states move toward the opposite end of the lightness scale.
    let (tint, hover_by, press_by) = if dark { (white, 0.14, 0.24) } else { (hex(0x000000), 0.10, 0.18) };
    let hover = |color: Hsla| mix(color, tint, hover_by);
    let pressed = |color: Hsla| mix(color, tint, press_by);

    c.background = p.card;
    c.foreground = p.tx;
    c.border = p.line;
    c.input = p.line;
    c.ring = p.acc;
    c.caret = if crate::prototype_fixture(){gpui::transparent_black()}else{p.acc};
    c.selection = p.accs;
    c.accent = p.card3;
    c.accent_foreground = p.tx;
    c.muted = p.card2;
    c.muted_foreground = p.tx3;
    c.popover = p.card;
    c.popover_foreground = p.tx;
    c.overlay = p.scrim;

    c.primary = p.acc;
    c.primary_hover = hover(p.acc);
    c.primary_active = pressed(p.acc);
    c.primary_foreground = white;
    c.secondary = p.card2;
    c.secondary_hover = p.card3;
    c.secondary_active = p.card3;
    c.secondary_foreground = p.tx;

    // Buttons: default and secondary are the soft `--card2` pill, primary is the accent.
    for (base, hovered, active, foreground) in [
        (&mut c.button, &mut c.button_hover, &mut c.button_active, &mut c.button_foreground),
        (
            &mut c.button_secondary,
            &mut c.button_secondary_hover,
            &mut c.button_secondary_active,
            &mut c.button_secondary_foreground,
        ),
    ] {
        *base = p.card2;
        *hovered = p.card3;
        *active = mix(p.card3, tint, 0.06);
        *foreground = p.tx;
    }
    for (base, hovered, active, foreground, color) in [
        (
            &mut c.button_primary,
            &mut c.button_primary_hover,
            &mut c.button_primary_active,
            &mut c.button_primary_foreground,
            p.acc,
        ),
        (&mut c.button_info, &mut c.button_info_hover, &mut c.button_info_active, &mut c.button_info_foreground, p.acc),
        (
            &mut c.button_danger,
            &mut c.button_danger_hover,
            &mut c.button_danger_active,
            &mut c.button_danger_foreground,
            p.err,
        ),
        (
            &mut c.button_success,
            &mut c.button_success_hover,
            &mut c.button_success_active,
            &mut c.button_success_foreground,
            p.ok,
        ),
        (
            &mut c.button_warning,
            &mut c.button_warning_hover,
            &mut c.button_warning_active,
            &mut c.button_warning_foreground,
            p.warn,
        ),
    ] {
        *base = color;
        *hovered = hover(color);
        *active = pressed(color);
        *foreground = white;
    }

    c.success = p.ok;
    c.success_hover = hover(p.ok);
    c.success_active = pressed(p.ok);
    c.success_foreground = white;
    c.warning = p.warn;
    c.warning_hover = hover(p.warn);
    c.warning_active = pressed(p.warn);
    c.warning_foreground = white;
    c.danger = p.err;
    c.danger_hover = hover(p.err);
    c.danger_active = pressed(p.err);
    c.danger_foreground = white;
    c.info = p.acc;
    c.info_hover = hover(p.acc);
    c.info_active = pressed(p.acc);
    c.info_foreground = white;

    c.link = p.acc;
    c.link_hover = hover(p.acc);
    c.link_active = pressed(p.acc);

    c.switch = p.card3;
    c.switch_thumb = white;

    c.list = p.card;
    c.list_even = p.card;
    c.list_head = p.card2;
    c.list_hover = p.card2;
    c.list_active = p.accs;
    c.list_active_border = p.acc;
    c.table = p.card;
    c.table_even = p.card;
    c.table_head = p.card2;
    c.table_head_foreground = p.tx3;
    c.table_foot = p.card2;
    c.table_foot_foreground = p.tx3;
    c.table_hover = p.card2;
    c.table_active = p.accs;
    c.table_active_border = p.acc;
    c.table_row_border = p.line;

    c.tab_bar = p.card2;
    c.tab_bar_segmented = p.card2;
    c.tab = p.card2;
    c.tab_active = p.card;
    c.tab_foreground = p.tx2;
    c.tab_active_foreground = p.tx;

    c.scrollbar_thumb = p.tx3.opacity(0.45);
    c.scrollbar_thumb_hover = p.tx3.opacity(0.7);
    c.drop_target = p.accs;
    c.drag_border = p.acc;
    c.progress_bar = p.acc;
    c.slider_bar = p.acc;
    c.slider_thumb = white;
}

/// Installs the light or dark tokens and mirrors them into gpui-component.
pub fn apply(dark: bool, cx: &mut App) {
    let palette = if dark { Palette::dark() } else { Palette::light() };
    let terminal = if dark { TerminalPalette::dark() } else { TerminalPalette::light() };

    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    let p = palette.clone();
    Theme::update(cx, |theme| {
        theme.font_family=UI_FONT.into();
        theme.font_size = px(13.);
        theme.mono_font_family = MONO_FONT.into();
        theme.radius = px(11.);
        theme.radius_lg = px(22.);
        theme.shadow = true;
        let ms=if crate::prototype_fixture(){0}else{180};
        theme.motion.duration_fast=std::time::Duration::from_millis(ms);
        theme.motion.duration_normal=std::time::Duration::from_millis(ms);
        theme.motion.duration_slow=std::time::Duration::from_millis(if ms==0{0}else{220});

        sync_component_colors(&mut theme.colors, &p, dark);
    });

    cx.set_global(AppTheme { dark, palette, terminal });
    cx.refresh_windows();
}
