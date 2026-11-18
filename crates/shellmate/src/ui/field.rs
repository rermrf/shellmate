use crate::theme::{ActivePalette, MONO_FONT, Palette};
use gpui::component::{
    Sizable,
    input::{Input, InputState},
};
use gpui::{prelude::*, *};

#[derive(Clone, Copy)]
pub enum Surface {
    Card,
    Card2,
    Window,
}

impl Surface {
    fn fill(self, p: &Palette) -> Hsla {
        match self {
            Self::Card => p.card2,
            Self::Card2 | Self::Window => p.card,
        }
    }
}

pub fn field_box<S: Styled>(element: S, surface: Surface, p: &Palette, focused: bool) -> S {
    element
        .min_h(px(38.))
        .max_h(px(38.))
        .px(px(12.))
        .py_0()
        .rounded(px(11.))
        .border_1()
        .border_color(if focused { p.acc } else { transparent_black() })
        .bg(surface.fill(p))
        .text_color(p.tx)
        .text_size(px(13.))
        .line_height(px(18.))
}

#[derive(IntoElement)]
pub struct FieldInput {
    state: Entity<InputState>,
    input: Input,
    surface: Surface,
    mono: bool,
}

pub fn field_input(state: &Entity<InputState>, surface: Surface, mono: bool) -> FieldInput {
    FieldInput {
        state: state.clone(),
        input: Input::new(state),
        surface,
        mono,
    }
}

impl FieldInput {
    pub fn mask_toggle(mut self) -> Self {
        self.input = self.input.mask_toggle();
        self
    }

    pub fn suffix(mut self, element: impl IntoElement) -> Self {
        self.input = self.input.suffix(element);
        self
    }
}

impl RenderOnce for FieldInput {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = self.state.read(cx).presentation();
        let disabled = state.is_disabled();
        let focused = !disabled && state.focus_handle().is_focused(window);
        let input = self
            .input
            .large()
            .bordered(false)
            .focus_bordered(false)
            .disabled(disabled)
            .readonly(state.is_readonly());
        let mut field = field_box(input, self.surface, cx.palette(), focused)
            .when(self.mono, |input| input.font_family(MONO_FONT));
        if disabled {
            field.style().background = None;
        }
        field
    }
}

#[cfg(test)]
mod tests {
    use super::{Palette, Surface};

    #[test]
    fn field_fill_contrasts_with_its_surface_in_both_themes() {
        for palette in [Palette::light(), Palette::dark()] {
            assert_eq!(Surface::Card.fill(&palette), palette.card2);
            assert_eq!(Surface::Card2.fill(&palette), palette.card);
            assert_eq!(Surface::Window.fill(&palette), palette.card);
        }
    }
}
