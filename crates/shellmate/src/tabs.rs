//! Tab-strip sizing from measured text widths. No UI or font-system dependency.

pub const TITLE_MAX: f32 = 190.;
pub const TAB_GAP: f32 = 6.;
pub const PLUS_WIDTH: f32 = 32.;
pub const OVERFLOW_GAP: f32 = 8.;

/// Padding, leading indicator, bounded title, optional chips and close control.
pub fn tab_width(title: f32, local: bool, reconnect: Option<f32>, broadcast: Option<f32>) -> f32 {
    12. + 6.
        + if local { 15. } else { 7. }
        + title.min(TITLE_MAX)
        + 22.
        + 2. * 8.
        + reconnect.map_or(0., |text| text + 10. + 8.)
        + broadcast.map_or(0., |text| text + 10. + 8.)
}

#[derive(Debug, PartialEq)]
pub struct Fit {
    pub visible: Vec<usize>,
    pub overflow: bool,
    /// The active tab may shrink only if a single tab cannot fit in the strip.
    pub single_tab_max: f32,
}

/// Reserve the overflow trigger only after determining that at least one tab must be hidden.
/// Maximize the visible count while preserving tab order and preferring earlier tabs; the active tab is mandatory.
/// `cap` is only for the ordinary prototype fixture's count-based sliding window.
pub fn fit_tabs(widths: &[f32], available: f32, overflow_width: f32, active: Option<usize>, cap: Option<usize>) -> Fit {
    let max_count = cap.unwrap_or(widths.len()).max(1);
    let plus_space = PLUS_WIDTH + if widths.is_empty() { 0. } else { TAB_GAP };
    let tab_budget = (available - plus_space).max(0.);
    let all = widths.iter().sum::<f32>() + TAB_GAP * widths.len().saturating_sub(1) as f32;
    if widths.len() <= max_count && all <= tab_budget {
        return Fit { visible: (0..widths.len()).collect(), overflow: false, single_tab_max: tab_budget };
    }
    let budget = (tab_budget - overflow_width - OVERFLOW_GAP).max(0.);
    let Some(active) = active.filter(|i| *i < widths.len()).or_else(|| (!widths.is_empty()).then_some(0)) else {
        return Fit { visible: vec![], overflow: false, single_tab_max: budget };
    };
    let mut visible = vec![active];
    let mut used = widths[active].min(budget);
    let start = if cap.is_some() { active.saturating_sub(max_count - 1) } else { 0 };
    let candidates = (start..widths.len()).chain(0..start).filter(|i| *i != active).collect::<Vec<_>>();
    // The cheapest remaining tabs determine the maximum attainable count. Then prefer earlier
    // tabs only when doing so still leaves room for that count (a wide early tab must not hide
    // several narrow ones). Render the chosen tabs in their original order.
    let cheapest = |indices: &[usize], count: usize| {
        let mut costs = indices.iter().map(|i| widths[*i] + TAB_GAP).collect::<Vec<_>>();
        costs.sort_by(f32::total_cmp);
        costs.into_iter().take(count).sum::<f32>()
    };
    let mut needed = (0..=candidates.len().min(max_count - 1))
        .take_while(|count| used + cheapest(&candidates, *count) <= budget)
        .last()
        .unwrap_or(0);
    for (position, index) in candidates.iter().copied().enumerate() {
        if needed == 0 {
            break;
        }
        let tail = &candidates[position + 1..];
        let cost = TAB_GAP + widths[index];
        if tail.len() >= needed - 1 && used + cost + cheapest(tail, needed - 1) <= budget {
            used += cost;
            visible.push(index);
            needed -= 1;
        }
    }
    visible.sort_unstable();
    let overflow = visible.len() < widths.len();
    Fit { visible, overflow, single_tab_max: if overflow { budget } else { tab_budget } }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Prototype DOM text boxes, measured in points: 13px title, 10px reconnect.
    fn prototype() -> Vec<f32> {
        vec![
            tab_width(83., false, None, None),
            tab_width(72., false, None, None),
            tab_width(88., false, None, None),
            tab_width(62., false, Some(38.), None),
        ]
    }
    fn available(window: f32) -> f32 {
        window - 284. - 12. - (70. + 57. + 94. + 32.) - 4. * 8.
    }
    #[test]
    fn four_prototype_tabs_fit_without_an_overflow_trigger_at_1440() {
        let fit = fit_tabs(&prototype(), available(1440.), 78., Some(0), None);
        assert_eq!(fit.visible, [0, 1, 2, 3]);
        assert!(!fit.overflow);
        // The drawer convention differs from physical fit; keep it explicit in the fixture.
        let drawer = fit_tabs(&prototype(), available(1440.), 78., Some(3), Some(3));
        assert_eq!(drawer.visible, [1, 2, 3]);
        assert!(drawer.overflow);
    }
    #[test]
    fn ordinary_local_fixture_slides_four_tabs_to_keep_the_active_one() {
        let mut widths = prototype();
        widths.push(tab_width(26., true, None, None));
        assert!(!fit_tabs(&widths, available(1440.), 78., Some(4), None).overflow);
        let fixture = fit_tabs(&widths, available(1440.), 78., Some(4), Some(4));
        assert_eq!(fixture.visible, [1, 2, 3, 4]);
        assert!(fixture.overflow);
    }
    #[test]
    fn long_tabs_fit_at_narrow_and_user_window_widths_with_active_visible() {
        let widths = vec![tab_width(500., false, None, None); 9];
        let narrow = fit_tabs(&widths, available(1100.), 78., Some(8), None);
        assert_eq!(narrow.visible, [8]);
        assert!(narrow.overflow);
        let wide = fit_tabs(&widths, available(1776.), 78., Some(8), None);
        assert_eq!(wide.visible, [0, 1, 2, 8]);
        assert!(wide.overflow);
        let short = fit_tabs(&prototype(), available(1100.), 78., Some(3), None);
        assert_eq!(short.visible, [0, 3]);
    }
    #[test]
    fn the_overflow_trigger_does_not_steal_space_before_it_is_needed() {
        let widths = [100., 100.];
        let exactly = 200. + TAB_GAP + PLUS_WIDTH + TAB_GAP;
        let all = fit_tabs(&widths, exactly, 90., Some(0), None);
        assert_eq!(all.visible, [0, 1]);
        assert!(!all.overflow);
        let hidden = fit_tabs(&widths, exactly - 0.5, 90., Some(1), None);
        assert_eq!(hidden.visible, [1]);
        assert!(hidden.overflow);
    }
    #[test]
    fn widths_include_local_icon_all_chips_and_the_bounded_title() {
        assert_eq!(tab_width(500., false, None, None), 253.);
        assert_eq!(tab_width(500., true, Some(38.), Some(20.)), 355.);
        assert_eq!(tab_width(10., true, None, None) - tab_width(10., false, None, None), 8.);
    }
    #[test]
    fn a_wide_early_tab_does_not_hide_three_tabs_that_fit() {
        let fit =
            fit_tabs(&[10., 200., 60., 60., 60.], 220. + PLUS_WIDTH + TAB_GAP + 78. + OVERFLOW_GAP, 78., Some(0), None);
        assert_eq!(fit.visible, [0, 2, 3, 4]);
        assert!(fit.overflow);
    }
    #[test]
    fn fit_never_hides_the_active_tab_or_reserves_a_spurious_menu() {
        let widths = [80., 240., 90., 180., 100.];
        for available in (80..1500).step_by(11) {
            for active in 0..widths.len() {
                let fit = fit_tabs(&widths, available as f32, 70., Some(active), None);
                assert!(fit.visible.contains(&active));
                assert_eq!(fit.overflow, fit.visible.len() < widths.len());
                assert!(fit.visible.windows(2).all(|w| w[0] < w[1]));
                if fit.visible.len() > 1 {
                    let used = fit.visible.iter().map(|i| widths[*i]).sum::<f32>()
                        + TAB_GAP * fit.visible.len() as f32
                        + PLUS_WIDTH
                        + if fit.overflow { 70. + OVERFLOW_GAP } else { 0. };
                    assert!(used <= available as f32);
                }
            }
        }
        assert_eq!(fit_tabs(&[], 100., 70., None, None).visible, Vec::<usize>::new());
        assert!(!fit_tabs(&[500.], 100., 70., Some(0), None).overflow);
    }
}
