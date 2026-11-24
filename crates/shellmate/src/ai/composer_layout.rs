//! M1 composer geometry, extended by the number of wrapped textarea rows.
pub(super) const VERTICAL_PADDING: f32 = 4.;
pub(super) const ACTION_HEIGHT: f32 = 30.;

pub(super) struct Layout {
    pub height: f32,
    pub viewport_height: f32,
    pub text_inset: f32,
}

pub(super) fn layout(rows: usize, rem: f32, line_height: f32) -> Layout {
    // M1 Input::h(32) is multi-line-only. Its single-line Medium input_h is
    // h_8 = 2 rem (26pt at Shellmate's 13pt rem), centered beside a 30pt button.
    let single_line_height = (2. * rem).max(ACTION_HEIGHT);
    let viewport_height = single_line_height + line_height * (rows.clamp(1, 8) - 1) as f32;
    Layout {
        height: viewport_height + 2. * VERTICAL_PADDING,
        viewport_height,
        text_inset: ((single_line_height - line_height) / 2.).max(0.),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_matches_m1_action_padding_and_text_baseline() {
        let line_height = 16.; // round(1.25 rem) at the app's 13pt rem.
        let m1_input_height = 2. * 13.;
        let m1_row_height = ACTION_HEIGHT.max(m1_input_height);
        let m1_text_top = VERTICAL_PADDING
            + (m1_row_height - m1_input_height) / 2.
            + (m1_input_height - line_height) / 2.;
        for rows in [0, 1] {
            let geometry = layout(rows, 13., line_height);
            assert_eq!(geometry.viewport_height, 30.);
            assert_eq!(geometry.height, 38.); // Full rounded bounds; not a column through a corner.
            assert_eq!(VERTICAL_PADDING + geometry.text_inset, m1_text_top);
            assert_eq!(40. - geometry.height, 2.); // Previous hard-coded 32 + 4 + 4.
        }
    }

    #[test]
    fn grows_by_wrapped_lines_and_caps_at_eight_without_moving_the_first_baseline() {
        let one = layout(1, 13., 16.);
        for rows in [2, 4, 8, 9, 100] {
            let geometry = layout(rows, 13., 16.);
            assert_eq!(geometry.height, one.height + 16. * (rows.min(8) - 1) as f32);
            assert_eq!(geometry.text_inset, one.text_inset);
            assert_eq!(
                geometry.viewport_height + 2. * VERTICAL_PADDING,
                geometry.height
            );
        }
    }
}
