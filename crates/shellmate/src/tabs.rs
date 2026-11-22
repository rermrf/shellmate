//! Tab-strip layout: every visible tab has the same width, the one the longest title asks for.
//! No UI or font-system dependency.

use std::cell::Cell;

pub const TAB_MAX: f32 = 220.;
pub const TAB_MIN: f32 = 120.;
/// The shared width moves in steps, so a title change rarely moves the tabs at all.
pub const TAB_STEP: f32 = 20.;
pub const TAB_GAP: f32 = 6.;
pub const PLUS_WIDTH: f32 = 32.;
pub const OVERFLOW_GAP: f32 = 8.;

/// The title keeps this much (about two CJK glyphs) before any chip gives way.
pub const TITLE_FLOOR: f32 = 28.;
// The parts of `Workspace::tab_face` around the title: padding, item gap, close control, chip padding.
const PADDING: f32 = 12. + 6.;
const ITEM_GAP: f32 = 8.;
const CLOSE: f32 = 22.;
const CHIP_PADDING: f32 = 10.;

#[derive(Debug, PartialEq)]
pub struct Layout {
    /// Indices of the tabs that are drawn, in their original order.
    pub visible: Vec<usize>,
    /// Some tab is hidden behind the session-list trigger.
    pub overflow: bool,
    /// Width of every visible tab.
    pub width: f32,
}

impl Layout {
    pub fn strip_width(&self) -> f32 {
        self.width * self.visible.len() as f32 + gaps(self.visible.len())
    }
}

fn gaps(count: usize) -> f32 {
    TAB_GAP * count.saturating_sub(1) as f32
}

/// Equal-width tabs: `target` while there is room, shrinking evenly down to `TAB_MIN`, then the
/// overflow trigger takes its share and the rest of the tabs go behind it. The trigger is reserved
/// only once a tab has to be hidden, so how many tabs are hidden depends on count and space alone.
/// The active tab is mandatory; earlier tabs are preferred.
/// `cap` is only for the ordinary prototype fixture's count-based sliding window.
/// `held` (the width kept while the pointer is over the tab bar) is honoured only if every tab fits
/// at that width without an overflow trigger.
pub fn layout(
    count: usize,
    available: f32,
    overflow_width: f32,
    active: Option<usize>,
    cap: Option<usize>,
    target: f32,
    held: Option<f32>,
) -> Layout {
    if count == 0 {
        return Layout { visible: vec![], overflow: false, width: 0. };
    }
    let target = target.clamp(TAB_MIN, TAB_MAX);
    let budget = (available - PLUS_WIDTH - TAB_GAP).max(0.);
    let limit = cap.unwrap_or(count).max(1);
    if count <= limit && count as f32 * TAB_MIN + gaps(count) <= budget {
        let equal = (budget - gaps(count)) / count as f32;
        let width = held
            .filter(|width| width.is_finite() && *width > 0. && *width * count as f32 + gaps(count) <= budget)
            .map_or(equal.min(target), |width| width.min(TAB_MAX));
        return Layout { visible: (0..count).collect(), overflow: false, width };
    }
    let room = (budget - overflow_width - OVERFLOW_GAP).max(0.);
    let fit = ((room + TAB_GAP) / (TAB_MIN + TAB_GAP)).floor() as usize;
    let shown = fit.clamp(1, limit.min(count));
    if shown == count {
        // A lone tab narrower than TAB_MIN: nothing is hidden, so there is no trigger to pay for.
        let width = ((budget - gaps(count)) / count as f32).clamp(0., target);
        return Layout { visible: (0..count).collect(), overflow: false, width };
    }
    let active = active.filter(|ix| *ix < count).unwrap_or(0);
    let start = if cap.is_some() { active.saturating_sub(limit - 1) } else { 0 };
    let mut visible =
        (start..count).chain(0..start).filter(|ix| *ix != active).take(shown - 1).chain([active]).collect::<Vec<_>>();
    visible.sort_unstable();
    let width = ((room - gaps(shown)) / shown as f32).clamp(0., target);
    Layout { visible, overflow: true, width }
}

/// What a tab shows next to its title; the widths are estimates or measurements in points.
#[derive(Clone, Copy, Debug)]
pub struct Content {
    /// The measured title (13 pt, semibold for every tab, active or not).
    pub title: f32,
    pub local: bool,
    /// Estimated width of the reconnect badge, if the session has one.
    pub badge: Option<f32>,
    /// The tab is in the broadcast set.
    pub broadcast: bool,
}

impl Content {
    /// Width at which the tab shows its chips and the whole title.
    pub fn width(self) -> f32 {
        width_with(self.title.max(TITLE_FLOOR), self.local, self.badge, self.broadcast)
    }
}

/// The width every tab asks for: the widest content over all sessions, rounded up to `TAB_STEP`
/// and kept within `TAB_MIN..=TAB_MAX`. Hidden sessions count too, so the width follows the
/// sessions and not what happens to be visible.
pub fn target_width(tabs: impl IntoIterator<Item = Content>) -> f32 {
    let widest = tabs.into_iter().map(Content::width).fold(0., f32::max);
    ((widest / TAB_STEP).ceil() * TAB_STEP).clamp(TAB_MIN, TAB_MAX)
}

/// The tab width held while the pointer is over the tab bar: neither a title change, a chip, a
/// close nor a new tab may move the tabs under the cursor. It starts with the width last laid out
/// and follows later layouts (which only differ when the held width no longer fits).
#[derive(Default)]
pub struct Hold {
    inside: Cell<bool>,
    rendered: Cell<f32>,
    held: Cell<Option<f32>>,
}

impl Hold {
    /// The width `layout` should keep, while the pointer is over the bar.
    pub fn held(&self) -> Option<f32> {
        self.held.get()
    }

    /// Records the width the tabs were just laid out with.
    pub fn rendered(&self, width: f32) {
        self.rendered.set(width);
        if self.inside.get() {
            self.held.set((width > 0.).then_some(width));
        }
    }

    /// The pointer moves over the bar: hold the width on screen (a no-op while already holding).
    pub fn enter(&self) {
        if !self.inside.replace(true) {
            let width = self.rendered.get();
            self.held.set((width > 0.).then_some(width));
        }
    }

    /// The pointer is gone (left the bar or the window, a drag began, the window went inactive).
    /// Returns whether it was holding, i.e. whether the tabs have to be laid out again.
    pub fn leave(&self) -> bool {
        self.held.take();
        self.inside.replace(false)
    }
}

/// Upper bound of a 10 pt chip's width: full-width glyphs take 1 em, everything else 0.6 em.
pub fn chip_estimate(text: &str) -> f32 {
    text.chars().map(|c| if c.is_ascii() { 6. } else { 10. }).sum::<f32>() + CHIP_PADDING
}

/// Padding, leading indicator, title, the given chips, close control and the gaps between them.
fn width_with(title: f32, local: bool, badge: Option<f32>, broadcast: bool) -> f32 {
    let chips = badge.into_iter().chain(broadcast.then(|| chip_estimate("广播")));
    PADDING
        + if local { 15. } else { 7. }
        + title
        + CLOSE
        + 2. * ITEM_GAP
        + chips.map(|chip| chip + ITEM_GAP).sum::<f32>()
}

/// Width a tab needs for the leading indicator, `TITLE_FLOOR`, the given chips and the close control.
fn needs(local: bool, badge: Option<f32>, broadcast: bool) -> f32 {
    width_with(TITLE_FLOOR, local, badge, broadcast)
}

/// A chip is shown whole or not at all (a clipped chip says nothing), and only if the title keeps
/// `TITLE_FLOOR` next to it. The reconnect badge shows only if it fits in full (`badge` is its
/// estimated width); the status dot and the session header carry the state when it does not.
pub fn badge_fits(width: f32, local: bool, badge: f32) -> bool {
    width >= needs(local, Some(badge), false)
}

/// The broadcast chip is the first thing to go when a tab is too narrow: it shows only if it fits
/// next to the title floor and the reconnect badge that is shown (`badge` is its estimated width).
pub fn broadcast_fits(width: f32, local: bool, badge: Option<f32>) -> bool {
    width >= needs(local, badge, true)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Chips {
    pub badge: bool,
    pub broadcast: bool,
}

/// Which optional chips a tab of `width` shows. `badge` is the estimated width of the reconnect
/// badge, if the session has one; `broadcast` says whether the tab is in the broadcast set. The
/// broadcast chip is judged next to the badge only when the badge is shown.
pub fn chips(width: f32, local: bool, badge: Option<f32>, broadcast: bool) -> Chips {
    let badge = badge.filter(|badge| badge_fits(width, local, *badge));
    Chips { badge: badge.is_some(), broadcast: broadcast && broadcast_fits(width, local, badge) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `available` that leaves exactly `budget` for the tabs when nothing is hidden.
    fn avail(budget: f32) -> f32 {
        budget + PLUS_WIDTH + TAB_GAP
    }
    /// `available` whose strip budget, after the trigger (78 wide), is `room`.
    fn avail_hidden(room: f32) -> f32 {
        avail(room + 78. + OVERFLOW_GAP)
    }
    /// `layout` with the widest possible target (what the equal-width tests of the cap and the
    /// thresholds assume); tests of the target call `super::layout` themselves.
    fn layout(
        count: usize,
        available: f32,
        overflow_width: f32,
        active: Option<usize>,
        cap: Option<usize>,
        held: Option<f32>,
    ) -> Layout {
        super::layout(count, available, overflow_width, active, cap, TAB_MAX, held)
    }
    /// A remote tab whose title is `title` pt wide.
    fn remote(title: f32) -> Content {
        Content { title, local: false, badge: None, broadcast: false }
    }
    /// A local tab whose title is `title` pt wide.
    fn local(title: f32) -> Content {
        Content { title, local: true, badge: None, broadcast: false }
    }
    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn few_tabs_stay_at_the_maximum_width() {
        let wide = layout(3, 1200., 78., Some(0), None, None);
        assert_eq!(wide, Layout { visible: vec![0, 1, 2], overflow: false, width: TAB_MAX });
        assert_eq!(wide.strip_width(), 3. * TAB_MAX + 2. * TAB_GAP);
        // Exactly full at the maximum.
        let full = layout(3, avail(3. * TAB_MAX + 2. * TAB_GAP), 78., Some(2), None, None);
        assert_eq!((full.width, full.overflow), (TAB_MAX, false));
        assert_eq!(layout(1, 1200., 78., Some(0), None, None).width, TAB_MAX);
    }

    #[test]
    fn tabs_shrink_evenly_down_to_the_minimum() {
        let layout4 = |budget| layout(4, avail(budget), 78., Some(1), None, None);
        let shrunk = layout4(4. * 150. + 3. * TAB_GAP);
        assert_eq!(shrunk, Layout { visible: vec![0, 1, 2, 3], overflow: false, width: 150. });
        assert_eq!(shrunk.strip_width(), 4. * 150. + 3. * TAB_GAP);
        let minimum = layout4(4. * TAB_MIN + 3. * TAB_GAP);
        assert_eq!((minimum.width, minimum.overflow, minimum.visible.len()), (TAB_MIN, false, 4));
    }

    #[test]
    fn overflow_starts_just_below_the_minimum() {
        let exact = avail(4. * TAB_MIN + 3. * TAB_GAP);
        assert!(!layout(4, exact, 78., Some(0), None, None).overflow);
        let below = layout(4, exact - 0.5, 78., Some(0), None, None);
        assert!(below.overflow);
        // 497.5 - 78 - 8 = 411.5 holds three tabs of (411.5 - 12) / 3.
        assert_eq!(below.visible, [0, 1, 2]);
        assert!(near(below.width, 399.5 / 3.));
        assert!(below.width >= TAB_MIN);
        // The trigger's own width moves the threshold between 3 and 2 tabs.
        let three = 3. * TAB_MIN + 2. * TAB_GAP;
        assert_eq!(layout(4, avail_hidden(three), 78., Some(0), None, None).visible.len(), 3);
        let two = layout(4, avail_hidden(three) - 0.5, 78., Some(0), None, None);
        assert_eq!(two.visible.len(), 2);
        assert!(two.width > TAB_MIN);
        assert!(near(two.strip_width(), three - 0.5));
    }

    #[test]
    fn the_active_tab_is_always_visible_and_earlier_tabs_are_preferred() {
        let available = avail_hidden(3. * TAB_MIN + 2. * TAB_GAP);
        let shown = |active| layout(9, available, 78., active, None, None);
        assert_eq!(shown(Some(8)).visible, [0, 1, 8]);
        assert_eq!(shown(Some(4)).visible, [0, 1, 4]);
        assert_eq!(shown(Some(1)).visible, [0, 1, 2]);
        assert_eq!(shown(Some(0)).visible, [0, 1, 2]);
        assert_eq!(shown(None).visible, [0, 1, 2]);
        assert_eq!(shown(Some(99)).visible, [0, 1, 2]);
        assert!(shown(Some(8)).overflow);
        assert_eq!(shown(Some(8)).width, TAB_MIN);
    }

    #[test]
    fn a_cap_slides_a_window_that_keeps_the_active_tab() {
        let wide = 5000.;
        let capped = |count, active, cap| layout(count, wide, 78., Some(active), Some(cap), None);
        let drawer = capped(4, 3, 3);
        assert_eq!(drawer, Layout { visible: vec![1, 2, 3], overflow: true, width: TAB_MAX });
        assert_eq!(capped(5, 4, 4).visible, [1, 2, 3, 4]);
        assert_eq!(capped(6, 2, 3).visible, [0, 1, 2]);
        assert_eq!(capped(6, 3, 3).visible, [1, 2, 3]);
        assert_eq!(capped(6, 5, 3).visible, [3, 4, 5]);
        // Within the cap and fitting: no window, no trigger.
        let fits = capped(4, 3, 4);
        assert_eq!((fits.visible, fits.overflow), (vec![0, 1, 2, 3], false));
        // Physical room still limits a capped window (it starts at the sliding start); a zero cap means one tab.
        assert_eq!(layout(6, avail_hidden(2. * TAB_MIN + TAB_GAP), 78., Some(5), Some(4), None).visible, [2, 5]);
        assert_eq!(capped(3, 1, 0).visible, [1]);
    }

    #[test]
    fn a_single_tab_may_be_narrower_than_the_minimum_but_never_negative() {
        let only = layout(1, avail(80.), 78., Some(0), None, None);
        assert_eq!(only, Layout { visible: vec![0], overflow: false, width: 80. });
        assert_eq!(layout(1, avail(500.), 78., Some(0), None, None).width, TAB_MAX);
        assert_eq!(layout(1, 10., 78., Some(0), None, None).width, 0.);
        // With hidden tabs the single visible one is what is left after the trigger.
        let narrow = layout(3, avail_hidden(100.), 78., Some(2), None, None);
        assert_eq!(narrow, Layout { visible: vec![2], overflow: true, width: 100. });
        assert_eq!(layout(3, 20., 78., Some(1), None, None), Layout { visible: vec![1], overflow: true, width: 0. });
    }

    #[test]
    fn a_held_width_is_kept_while_every_tab_fits_at_it() {
        // Three tabs would grow to 170 after a close; 150 was the width before.
        let budget = 3. * 170. + 2. * TAB_GAP;
        let kept = layout(3, avail(budget), 78., Some(0), None, Some(150.));
        assert_eq!(kept, Layout { visible: vec![0, 1, 2], overflow: false, width: 150. });
        assert_eq!(layout(3, avail(budget), 78., Some(0), None, None).width, 170.);
        // Exactly fitting is fine; a hair too wide is ignored.
        assert_eq!(layout(3, avail(budget), 78., Some(0), None, Some(170.)).width, 170.);
        assert!(near(layout(3, avail(budget - 0.5), 78., Some(0), None, Some(170.)).width, (budget - 0.5 - 12.) / 3.));
        // It never grows tabs beyond the maximum or accepts nonsense.
        assert_eq!(layout(2, 5000., 78., Some(0), None, Some(300.)).width, TAB_MAX);
        for bad in [0., -5., f32::NAN, f32::INFINITY] {
            assert_eq!(layout(3, avail(budget), 78., Some(0), None, Some(bad)).width, 170.);
        }
    }

    #[test]
    fn a_held_width_is_ignored_when_it_does_not_fit_or_tabs_overflow() {
        let tight = avail(4. * 150. + 3. * TAB_GAP);
        assert_eq!(layout(4, tight, 78., Some(0), None, Some(200.)).width, 150.);
        // Overflowing layouts follow the room, not the held width.
        let hidden = layout(9, avail_hidden(3. * TAB_MIN + 2. * TAB_GAP), 78., Some(0), None, Some(150.));
        assert_eq!((hidden.width, hidden.overflow), (TAB_MIN, true));
        // Capped windows overflow too.
        assert_eq!(layout(6, 5000., 78., Some(5), Some(3), Some(150.)).width, TAB_MAX);
        // Without enough room for even TAB_MIN a held width cannot force everything to fit.
        let below = layout(4, avail(4. * TAB_MIN + 3. * TAB_GAP - 0.5), 78., Some(0), None, Some(TAB_MIN));
        assert!(below.overflow);
    }

    #[test]
    fn a_held_width_may_exceed_a_target_that_shrank_meanwhile() {
        // A title got shorter while the pointer was over the bar: the tabs stay as wide as they were.
        let wide = super::layout(3, 1200., 78., Some(0), None, 160., Some(200.));
        assert_eq!(wide.width, 200.);
        assert_eq!(super::layout(3, 1200., 78., Some(0), None, 160., None).width, 160.);
        // And a longer title does not widen them.
        assert_eq!(super::layout(3, 1200., 78., Some(0), None, 220., Some(160.)).width, 160.);
    }

    #[test]
    fn no_tabs_make_an_empty_layout() {
        for active in [None, Some(0)] {
            assert_eq!(
                layout(0, 800., 78., active, None, None),
                Layout { visible: vec![], overflow: false, width: 0. }
            );
            assert_eq!(super::layout(0, 800., 78., active, None, 160., Some(150.)).width, 0.);
            assert_eq!(layout(0, 800., 78., active, Some(4), Some(150.)).visible, Vec::<usize>::new());
        }
        assert_eq!(layout(0, 0., 78., None, None, None).strip_width(), 0.);
    }

    #[test]
    fn layout_invariants_hold_across_sizes() {
        for count in 0..12usize {
            for available in (0..1800).step_by(13) {
                for active in [None, Some(0), Some(count / 2), Some(count.saturating_sub(1))] {
                    for (cap, held, target) in [
                        (None, None, TAB_MAX),
                        (None, Some(150.), TAB_MAX),
                        (Some(4), None, 160.),
                        (Some(3), Some(TAB_MIN), TAB_MIN),
                        (None, None, 140.),
                        (None, Some(200.), 180.),
                    ] {
                        let l = super::layout(count, available as f32, 70., active, cap, target, held);
                        assert!(l.visible.len() <= count);
                        assert_eq!(l.overflow, l.visible.len() < count);
                        assert!(l.visible.windows(2).all(|w| w[0] < w[1]));
                        assert!(l.visible.iter().all(|ix| *ix < count));
                        assert!((0.0..=TAB_MAX).contains(&l.width));
                        // Never wider than asked for, unless a held width was kept.
                        assert!(l.width <= target.max(held.unwrap_or(0.)), "{count} {available} {l:?}");
                        if count > 0 {
                            assert!(l.visible.contains(&active.filter(|a| *a < count).unwrap_or(0)));
                            assert!(!l.visible.is_empty());
                        }
                        if let Some(cap) = cap {
                            assert!(l.visible.len() <= cap.max(1));
                        }
                        if l.visible.len() > 1 {
                            assert!(l.width >= TAB_MIN);
                            let used = l.strip_width()
                                + TAB_GAP
                                + PLUS_WIDTH
                                + if l.overflow { 70. + OVERFLOW_GAP } else { 0. };
                            assert!(used <= available as f32 + 1e-3, "{count} {available} {l:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn content_width_is_the_sum_of_the_tab_parts() {
        // Padding 18, laptop 15, title, close 22 and two gaps of 8.
        assert_eq!(local(74.).width(), 18. + 15. + 74. + 22. + 16.);
        assert_eq!(remote(74.).width(), 18. + 7. + 74. + 22. + 16.);
        let both = Content { title: 74., local: false, badge: Some(54.), broadcast: true };
        assert_eq!(both.width(), remote(74.).width() + (54. + 8.) + (30. + 8.));
    }

    #[test]
    fn the_target_follows_the_longest_title_in_steps() {
        // 「emoji — zsh」 is about 74 pt: 145 pt of tab, rounded up to 160 instead of 220.
        assert_eq!(local(74.).width(), 145.);
        assert_eq!(target_width([local(74.), local(74.), local(74.)]), 160.);
        // A tab of exactly 178 pt rounds up to 180.
        assert_eq!(remote(178. - 63.).width(), 178.);
        assert_eq!(target_width([remote(178. - 63.)]), 180.);
        // The widest of all sessions wins, whatever their order.
        assert_eq!(target_width([local(74.), local(100.), local(74.)]), 180.);
        assert_eq!(target_width([local(100.), local(74.)]), target_width([local(74.), local(100.)]));
    }

    #[test]
    fn the_target_counts_the_chips_and_the_title_floor() {
        let plain = remote(60.);
        let badge = Content { badge: Some(chip_estimate("重连 3/5")), ..plain };
        let broadcast = Content { broadcast: true, ..plain };
        let both = Content { broadcast: true, ..badge };
        assert_eq!(plain.width(), 123.);
        assert_eq!(target_width([plain]), 140.);
        assert_eq!(target_width([badge]), 200.);
        assert_eq!(target_width([broadcast]), 180.);
        assert_eq!(target_width([both]), TAB_MAX);
        assert!(target_width([plain, both]) > target_width([plain]));
        // A very short title (or none) is worth the title floor.
        for title in [0., 1., 10., TITLE_FLOOR] {
            assert_eq!(local(title).width(), local(TITLE_FLOOR).width());
        }
        assert_eq!(local(TITLE_FLOOR).width(), 18. + 15. + TITLE_FLOOR + 22. + 16.);
    }

    #[test]
    fn the_target_is_clamped_and_exact_steps_are_not_bumped() {
        // Short titles still get TAB_MIN, long ones TAB_MAX, and no sessions at all the minimum.
        assert_eq!(target_width([local(0.)]), TAB_MIN);
        assert_eq!(target_width([remote(10.)]), TAB_MIN);
        assert_eq!(target_width([local(500.)]), TAB_MAX);
        assert_eq!(target_width(Vec::<Content>::new()), TAB_MIN);
        // 71 pt of fixed parts: a title of 89 makes exactly 160, which is already a multiple of the step.
        assert_eq!(local(89.).width(), 160.);
        assert_eq!(target_width([local(89.)]), 160.);
        assert_eq!(target_width([local(89.5)]), 180.);
        assert_eq!(target_width([local(TAB_MAX - 71.)]), TAB_MAX);
        assert_eq!(target_width([local(TAB_MIN - 71.)]), TAB_MIN);
        assert_eq!(target_width([local(TAB_MIN - 70.5)]), 140.);
    }

    #[test]
    fn a_tab_shows_its_chips_and_whole_title_at_its_content_width() {
        let badge = chip_estimate("重连 3/5");
        for local in [false, true] {
            for badge in [None, Some(badge), Some(chip_estimate("连接失败"))] {
                for broadcast in [false, true] {
                    for title in [0., 40., 100.] {
                        let content = Content { title, local, badge: badge.filter(|_| !local), broadcast };
                        let shown = chips(content.width(), local, content.badge, broadcast);
                        assert_eq!(shown, Chips { badge: content.badge.is_some(), broadcast }, "{content:?}");
                        // The title gets everything the chips leave: at least its own width.
                        let chips_width = if shown.badge { content.badge.unwrap_or(0.) + ITEM_GAP } else { 0. }
                            + if shown.broadcast { chip_estimate("广播") + ITEM_GAP } else { 0. };
                        let room = content.width() - PADDING - if local { 15. } else { 7. } - CLOSE - 2. * ITEM_GAP;
                        assert!(room - chips_width >= title.max(TITLE_FLOOR) - 1e-3);
                    }
                }
            }
        }
        // Rounded up to a step the chips are still there.
        let both = Content { title: 40., local: false, badge: Some(badge), broadcast: true };
        let stepped = target_width([both]);
        assert!(stepped >= both.width());
        assert_eq!(chips(stepped, false, both.badge, true), Chips { badge: true, broadcast: true });
    }

    #[test]
    fn a_target_below_the_maximum_sets_the_tab_width_while_there_is_room() {
        let lay = |count, available, active, cap, target, held| {
            super::layout(count, available, 78., active, cap, target, held)
        };
        // Plenty of room: three tabs of 160, not 220.
        let few = lay(3, 1200., Some(0), None, 160., None);
        assert_eq!(few, Layout { visible: vec![0, 1, 2], overflow: false, width: 160. });
        assert_eq!(few.strip_width(), 3. * 160. + 2. * TAB_GAP);
        // A target smaller than the equal share wins; a larger one yields to the share.
        assert_eq!(lay(4, avail(4. * 200. + 3. * TAB_GAP), Some(0), None, 160., None).width, 160.);
        assert_eq!(lay(4, avail(4. * 150. + 3. * TAB_GAP), Some(0), None, 180., None).width, 150.);
        assert_eq!(lay(4, avail(4. * 180. + 3. * TAB_GAP), Some(0), None, 180., None).width, 180.);
        // Crowded strips still shrink down to TAB_MIN and then overflow exactly as before.
        let minimum = 4. * TAB_MIN + 3. * TAB_GAP;
        assert_eq!(lay(4, avail(minimum), Some(0), None, 160., None).width, TAB_MIN);
        assert!(!lay(4, avail(minimum), Some(0), None, 160., None).overflow);
        assert!(lay(4, avail(minimum) - 0.5, Some(0), None, 160., None).overflow);
        assert_eq!(
            lay(4, avail(minimum) - 0.5, Some(0), None, 160., None).visible,
            lay(4, avail(minimum) - 0.5, Some(0), None, TAB_MAX, None).visible
        );
    }

    #[test]
    fn overflow_hides_the_same_tabs_whatever_the_target() {
        let available = avail_hidden(3. * TAB_MIN + 2. * TAB_GAP);
        for target in [TAB_MIN, 160., TAB_MAX] {
            let l = super::layout(9, available, 78., Some(8), None, target, None);
            assert_eq!(l.visible, [0, 1, 8]);
            assert!(l.overflow);
            assert_eq!(l.width, TAB_MIN);
        }
        // With room to spare the target is the ceiling of what the visible tabs get.
        let spare = super::layout(6, 5000., 78., Some(5), Some(3), 160., None);
        assert_eq!(spare, Layout { visible: vec![3, 4, 5], overflow: true, width: 160. });
        // One visible tab among hidden ones: the room after the trigger, up to the target.
        let one = |target| super::layout(3, avail_hidden(200.), 78., Some(2), None, target, None);
        assert_eq!(one(160.).width, 160.);
        assert_eq!(one(TAB_MAX).width, 200.);
        assert_eq!(one(160.).visible, [2]);
    }

    #[test]
    fn a_target_outside_the_range_is_clamped() {
        assert_eq!(super::layout(2, 1200., 78., Some(0), None, 50., None).width, TAB_MIN);
        assert_eq!(super::layout(2, 1200., 78., Some(0), None, 900., None).width, TAB_MAX);
    }

    #[test]
    fn the_hold_starts_with_the_width_on_screen_and_ends_with_the_pointer() {
        let hold = Hold::default();
        // Nothing is held while the pointer is elsewhere, whatever is laid out.
        hold.rendered(160.);
        assert_eq!(hold.held(), None);
        assert!(!hold.leave());
        hold.enter();
        assert_eq!(hold.held(), Some(160.));
        // Entering again (every mouse move does) neither re-captures nor changes anything.
        hold.enter();
        assert_eq!(hold.held(), Some(160.));
        // Leaving reports that the tabs must be laid out again, once.
        assert!(hold.leave());
        assert_eq!(hold.held(), None);
        assert!(!hold.leave());
        // After leaving, the next layout is free and the next enter captures that one.
        hold.rendered(200.);
        assert_eq!(hold.held(), None);
        hold.enter();
        assert_eq!(hold.held(), Some(200.));
    }

    #[test]
    fn the_hold_follows_a_layout_that_could_not_keep_it() {
        let hold = Hold::default();
        hold.rendered(160.);
        hold.enter();
        hold.rendered(130.);
        assert_eq!(hold.held(), Some(130.));
        // Entering with no tabs holds nothing yet; the first tab that appears sets the width.
        let empty = Hold::default();
        empty.rendered(0.);
        empty.enter();
        assert_eq!(empty.held(), None);
        empty.rendered(180.);
        assert_eq!(empty.held(), Some(180.));
        empty.rendered(0.);
        assert_eq!(empty.held(), None);
        assert!(empty.leave());
    }

    /// Lays out like the workspace does on every render and returns the width.
    fn render(hold: &Hold, count: usize, available: f32, target: f32) -> Layout {
        let l = super::layout(count, available, 78., Some(0), None, target, hold.held());
        hold.rendered(l.width);
        l
    }

    #[test]
    fn while_the_pointer_is_over_the_bar_closing_adding_and_title_changes_do_not_move_the_tabs() {
        let available = avail(5. * 400.);
        let hold = Hold::default();
        let start = render(&hold, 4, available, 160.);
        assert_eq!(start.width, 160.);
        hold.enter();
        // Closing with ×: three tabs would still be 160 pt wide, but a shorter longest title asks for 140.
        assert_eq!(render(&hold, 3, available, 140.).width, 160.);
        assert_eq!(render(&hold, 2, available, 120.).width, 160.);
        // Adding with 「+」 and a longer title does not widen them either.
        assert_eq!(render(&hold, 3, available, 200.).width, 160.);
        assert_eq!(render(&hold, 6, available, TAB_MAX).width, 160.);
        // Leaving frees the layout: the next render follows the target.
        assert!(hold.leave());
        assert_eq!(render(&hold, 6, available, TAB_MAX).width, TAB_MAX);
        assert_eq!(render(&hold, 6, available, 180.).width, 180.);
    }

    #[test]
    fn a_hold_that_stops_fitting_gives_way_and_the_hold_adopts_the_new_width() {
        // Six tabs fit at 160 pt but not at 200 pt: the hold must give way for the sixth.
        let available = avail(6. * 160. + 5. * TAB_GAP);
        let hold = Hold::default();
        assert_eq!(render(&hold, 3, available, 200.).width, 200.);
        hold.enter();
        assert_eq!(render(&hold, 4, available, 200.).width, 200.);
        let crowded = render(&hold, 6, available, 200.);
        assert_eq!(crowded.width, 160.);
        assert!(!crowded.overflow);
        // The new width is what is held now, so closing a tab does not bring 200 pt back.
        assert_eq!(hold.held(), Some(160.));
        assert_eq!(render(&hold, 5, available, 200.).width, 160.);
        // Overflow ignores the hold too, and the hold follows the overflowing width.
        let overflowing = render(&hold, 12, available, 200.);
        assert!(overflowing.overflow);
        assert_eq!(hold.held(), Some(overflowing.width));
    }

    #[test]
    fn chip_estimates_never_undershoot_the_ten_point_text() {
        assert_eq!(chip_estimate("广播"), 30.);
        // 重连 3/5 and 连接失败 measure about 48 and 50 in the UI font.
        assert!(chip_estimate("重连 3/5") >= 48.);
        assert!(chip_estimate("连接失败") >= 50.);
    }

    #[test]
    fn the_broadcast_chip_yields_before_the_title_floor_or_the_close_button() {
        let badge = Some(chip_estimate("重连 3/5"));
        // At TAB_MIN neither a remote nor a local tab can show it next to the badge or a full title floor.
        assert!(!broadcast_fits(TAB_MIN, false, badge));
        assert!(!broadcast_fits(TAB_MIN, false, None));
        assert!(!broadcast_fits(TAB_MIN, true, None));
        assert!(broadcast_fits(220., false, badge));
        assert!(broadcast_fits(TAB_MAX, true, None));
        // The threshold is the sum of the parts: padding, dot, floor, chip, close and three gaps.
        let remote = PADDING + 7. + TITLE_FLOOR + 30. + CLOSE + 3. * ITEM_GAP;
        assert!(broadcast_fits(remote, false, None));
        assert!(!broadcast_fits(remote - 0.5, false, None));
    }

    #[test]
    fn the_title_floor_and_close_button_fit_the_minimum_without_chips() {
        assert!(needs(false, None, false) <= TAB_MIN);
        assert!(needs(true, None, false) <= TAB_MIN);
        assert_eq!(needs(true, None, false) - needs(false, None, false), 8.);
    }

    #[test]
    fn the_reconnect_badge_hides_at_the_minimum_and_shows_at_the_maximum() {
        for text in ["重连 3/5", "连接失败"] {
            let badge = chip_estimate(text);
            assert!(!badge_fits(TAB_MIN, false, badge), "{text}");
            assert!(badge_fits(TAB_MAX, false, badge), "{text}");
            assert_eq!(chips(TAB_MIN, false, Some(badge), false), Chips { badge: false, broadcast: false });
            assert_eq!(chips(TAB_MAX, false, Some(badge), false), Chips { badge: true, broadcast: false });
        }
        // Not even a stretch of 150 pt carries 「重连 3/5」 in full, but 「连接失败」 fits there.
        assert!(!badge_fits(150., false, chip_estimate("重连 3/5")));
        assert!(badge_fits(150., false, chip_estimate("连接失败")));
    }

    #[test]
    fn the_reconnect_badge_threshold_is_the_sum_of_its_parts() {
        let badge = chip_estimate("重连 3/5");
        // Padding, dot, title floor, badge, close and three gaps.
        let remote = PADDING + 7. + TITLE_FLOOR + badge + CLOSE + 3. * ITEM_GAP;
        assert!(badge_fits(remote, false, badge));
        assert!(!badge_fits(remote - 0.5, false, badge));
        // The laptop icon is 8 pt wider than the dot.
        assert!(badge_fits(remote + 8., true, badge));
        assert!(!badge_fits(remote + 7.5, true, badge));
        assert!(chips(remote, false, Some(badge), false).badge);
        assert!(!chips(remote - 0.5, false, Some(badge), false).badge);
    }

    #[test]
    fn the_broadcast_chip_is_judged_next_to_the_badge_only_when_the_badge_is_shown() {
        let badge = chip_estimate("重连 3/5");
        let with_badge = needs(false, Some(badge), true);
        let alone = needs(false, None, true);
        assert!(alone < needs(false, Some(badge), false) && needs(false, Some(badge), false) < with_badge);
        // Room for both: both show.
        assert_eq!(chips(with_badge, false, Some(badge), true), Chips { badge: true, broadcast: true });
        // The badge fits but the pair does not: the badge stays, the broadcast chip goes.
        assert_eq!(chips(with_badge - 0.5, false, Some(badge), true), Chips { badge: true, broadcast: false });
        // The badge is hidden for lack of room, but the broadcast chip fits on its own and shows.
        let between = needs(false, Some(badge), false) - 0.5;
        assert!(between >= alone);
        assert_eq!(chips(between, false, Some(badge), true), Chips { badge: false, broadcast: true });
        // Neither fits.
        assert_eq!(chips(alone - 0.5, false, Some(badge), true), Chips { badge: false, broadcast: false });
        // At TAB_MIN both are hidden for a remote tab with either badge.
        for text in ["重连 3/5", "连接失败"] {
            assert_eq!(
                chips(TAB_MIN, false, Some(chip_estimate(text)), true),
                Chips { badge: false, broadcast: false }
            );
        }
        // A tab that is not in the broadcast set never shows the chip; one without a badge asks for none.
        assert!(!chips(TAB_MAX, false, Some(badge), false).broadcast);
        assert_eq!(chips(TAB_MAX, true, None, true), Chips { badge: false, broadcast: true });
    }

    #[test]
    fn the_close_button_always_fits_next_to_whatever_chips_are_shown() {
        for text in ["重连 3/5", "连接失败"] {
            let estimate = chip_estimate(text);
            for local in [false, true] {
                for width in (0..2400).map(|n| n as f32 / 8.) {
                    for (badge, broadcast) in
                        [(None, false), (None, true), (Some(estimate), false), (Some(estimate), true)]
                    {
                        let shown = chips(width, local, badge, broadcast);
                        assert!(!shown.badge || badge.is_some());
                        assert!(!shown.broadcast || broadcast);
                        // Even with the title squeezed to nothing (below TAB_MIN) everything shown must fit.
                        let gaps = 2 + usize::from(shown.badge) + usize::from(shown.broadcast);
                        let used = PADDING
                            + if local { 15. } else { 7. }
                            + CLOSE
                            + ITEM_GAP * gaps as f32
                            + if shown.badge { estimate } else { 0. }
                            + if shown.broadcast { chip_estimate("广播") } else { 0. };
                        // (A tab narrower than the bare dot, title and close button shows no chip at all.)
                        assert!(
                            used <= width || shown == Chips { badge: false, broadcast: false },
                            "{width} {local} {shown:?}"
                        );
                    }
                }
            }
        }
    }
}
