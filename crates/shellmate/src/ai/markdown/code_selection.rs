//! A precise window-selection participant for the text inside a custom code block.
//! The underlying TextView still paints syntax highlighting. This transparent
//! text layer paints only selection, using the same mono font/size/line height.
use crate::theme::{ActivePalette, MONO_FONT};
use gpui::{
    base::{SelectableText, TextSelectionHandle, TextSelectionRegistration},
    prelude::*,
    *,
};

struct State {
    selection: TextSelectionHandle,
    focus: FocusHandle,
    _refresh: Subscription,
}
impl State {
    fn new(w: &mut Window, cx: &mut Context<Self>) -> Self {
        let selection = TextSelectionHandle::new("", cx);
        let focus = cx.focus_handle();
        let on_focus = focus.clone();
        selection.focus_with(move |w, cx| on_focus.focus(w, cx), cx);
        let refresh = selection.refresh_window_on_change(w, cx);
        Self {
            selection,
            focus,
            _refresh: refresh,
        }
    }
}

/// Full content can be wider than the whole answer. GPUI chooses the smallest
/// hovered participant, so registering full content lets the outer atomic
/// Markdown block win. Use the visible bounds and carry scrolling separately.
fn geometry(
    content: Bounds<Pixels>,
    clip: Bounds<Pixels>,
) -> Option<(Bounds<Pixels>, Point<Pixels>)> {
    let intersection = content.intersect(&clip);
    if intersection.size.width <= px(0.) || intersection.size.height <= px(0.) {
        return None;
    }
    // Keep the row's trailing whitespace in the hitbox. At the rightmost
    // scroll offset, the edge-projected pointer is past the last glyph but
    // must still extend this text to its end, not switch to the outer block.
    let visible = Bounds::new(
        point(clip.left(), intersection.top()),
        size(clip.size.width, intersection.size.height),
    );
    Some((visible, content.origin - visible.origin))
}

struct CodeSelection {
    text: SelectableText,
    handle: TextSelectionHandle,
}
impl IntoElement for CodeSelection {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for CodeSelection {
    type RequestLayoutState = <SelectableText as Element>::RequestLayoutState;
    type PrepaintState = <SelectableText as Element>::PrepaintState;
    fn id(&self) -> Option<ElementId> {
        self.text.id()
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        w: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.text.request_layout(id, inspector, w, cx)
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        w: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = self.text.prepaint(id, inspector, bounds, layout, w, cx);
        if let Some((visible, offset)) = geometry(bounds, w.content_mask().bounds) {
            // Replace this same handle's default full-content registration.
            // This normal row hitbox includes trailing whitespace; only the
            // small copy button occludes it.
            let row_hitbox = w.insert_hitbox(visible, HitboxBehavior::Normal);
            self.handle.register(
                TextSelectionRegistration::new(row_hitbox, visible)
                    .with_scroll_offset(offset)
                    .with_text_bounds(vec![bounds])
                    .with_rendered_element(&self.handle, w, cx),
                w,
                cx,
            );
        }
        hitbox
    }
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        w: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .paint(id, inspector, bounds, layout, hitbox, w, cx);
    }
}

pub(super) fn overlay(
    id: SharedString,
    code: String,
    w: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let state = w.use_keyed_state(
        SharedString::from(format!("{id}:selection-state")),
        cx,
        State::new,
    );
    let handle = state.read(cx).selection.clone();
    let text = SelectableText::with_handle("code-selection", handle.clone(), code)
        .text_style(TextStyleRefinement {
            color: Some(transparent_black()),
            ..Default::default()
        })
        .selection_color(cx.palette().accs);
    div()
        .absolute()
        .inset_0()
        .p(px(11.))
        .font_family(MONO_FONT)
        .text_size(px(12.5))
        .line_height(px(18.))
        .whitespace_nowrap()
        .track_focus(&state.read(cx).focus)
        .child(CodeSelection { text, handle })
}

#[cfg(test)]
mod tests {
    use super::geometry;
    use gpui::{Bounds, point, px, size};
    #[test]
    fn long_code_hit_area_stays_smaller_than_the_parent_and_offsets_preserve_anchor() {
        let clip = Bounds::new(point(px(1102.), px(259.5)), size(px(312.), px(40.)));
        let text = Bounds::new(point(px(1113.), px(270.5)), size(px(1315.), px(18.)));
        let (visible, offset) = geometry(text, clip).unwrap();
        assert!(visible.contains(&point(px(1114.), px(280.))));
        assert_eq!(visible.size, size(px(312.), px(18.)));
        assert_eq!(visible.origin + offset, text.origin);
        let shifted = Bounds::new(text.origin - point(px(1025.), px(0.)), text.size);
        let (visible, offset) = geometry(shifted, clip).unwrap();
        assert_eq!(visible.origin + offset, shifted.origin);
        assert_eq!(visible.right(), clip.right());
        assert!(
            visible.contains(&point(clip.right() - px(1.), px(280.))),
            "the final pointer stays in this participant after the last glyph"
        );
        // Endpoint storage subtracts origin + scroll_offset, so the logical
        // character coordinate survives the full horizontal scroll distance.
        assert_eq!(
            text.origin + point(px(8.), px(9.)) - text.origin,
            shifted.origin + point(px(8.), px(9.)) - visible.origin - offset
        );
        assert!(f32::from(visible.size.width) * f32::from(visible.size.height) < 312. * 100.);
    }
    #[test]
    fn clipped_away_text_has_no_visible_registration() {
        assert!(
            geometry(
                Bounds::new(point(px(-200.), px(0.)), size(px(100.), px(18.))),
                Bounds::new(point(px(0.), px(0.)), size(px(100.), px(100.)))
            )
            .is_none()
        );
    }
}
