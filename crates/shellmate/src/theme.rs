//! Design tokens from the prototype (`THEME` / `TERM`), plus syncing them into
//! gpui-component's theme so its widgets match.

use gpui::component::{Theme, ThemeMode};
use gpui::{App, Global, Hsla, Pixels, Rgba, SharedString, px};
use shellmate_core::GroupTone;
use shellmate_term::{TerminalPalette, TerminalStyle};

pub const MONO_FONT: &str = "JetBrains Mono";
pub const CJK_FALLBACK: &str = "PingFang SC";

/// UI color tokens. Field names follow the prototype's CSS variables.
#[allow(dead_code)] // the full token set is kept even where P0 doesn't use it yet
#[derive(Clone, Debug)]
pub struct Palette {
    /// Window ground (`--win`).
    pub win: Hsla,
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
}

fn hex(v: u32) -> Hsla {
    rgba(v, 1.)
}

fn rgba(v: u32, a: f32) -> Hsla {
    Rgba {
        r: ((v >> 16) & 0xff) as f32 / 255.,
        g: ((v >> 8) & 0xff) as f32 / 255.,
        b: (v & 0xff) as f32 / 255.,
        a,
    }
    .into()
}

impl Palette {
    pub fn light() -> Self {
        Self {
            win: hex(0xEEF0F4),
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
            shadow: rgba(0x101828, 0.07),
        }
    }

    pub fn dark() -> Self {
        Self {
            win: hex(0x111215),
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
            shadow: rgba(0x000000, 0.28),
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

/// Installs the light or dark tokens and mirrors them into gpui-component.
pub fn apply(dark: bool, cx: &mut App) {
    let palette = if dark { Palette::dark() } else { Palette::light() };
    let terminal = if dark { TerminalPalette::dark() } else { TerminalPalette::light() };

    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    let p = palette.clone();
    Theme::update(cx, |theme| {
        theme.font_size = px(14.);
        theme.mono_font_family = MONO_FONT.into();
        theme.radius = px(8.);
        theme.radius_lg = px(12.);
        theme.shadow = true;

        let c = &mut theme.colors;
        c.background = p.card;
        c.foreground = p.tx;
        c.border = p.line;
        c.input = p.line;
        c.ring = p.acc;
        c.primary = p.acc;
        c.primary_hover = p.acc.opacity(0.9);
        c.primary_active = p.acc.opacity(0.8);
        c.primary_foreground = hex(0xFFFFFF);
        c.secondary = p.card2;
        c.secondary_hover = p.card3;
        c.secondary_active = p.card3;
        c.secondary_foreground = p.tx;
        c.muted = p.card2;
        c.muted_foreground = p.tx3;
        c.accent = p.card3;
        c.accent_foreground = p.tx;
        c.popover = p.card;
        c.popover_foreground = p.tx;
        c.selection = p.accs;
        c.caret = p.acc;
        c.success = p.ok;
        c.warning = p.warn;
        c.danger = p.err;
    });

    cx.set_global(AppTheme { dark, palette, terminal });
    cx.refresh_windows();
}
