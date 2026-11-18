use gpui::{App, Context, ElementId, IntoElement, Window, canvas, prelude::*};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};
pub(crate) mod blocks;

pub(crate) const CHROME_DURATION: Duration = Duration::from_millis(220);

pub(crate) fn ease_out(time: f32) -> f32 {
    1. - (1. - time).powi(3)
}

pub(crate) fn mix(start: f32, end: f32, progress: f32) -> f32 {
    start + (end - start) * progress
}

pub(crate) fn enabled(cx: &gpui::App) -> bool {
    !cx.reduce_motion() && !(cfg!(debug_assertions) && std::env::var_os("SHELLMATE_DEMO").is_some())
}

pub(crate) fn refresh_preference(_cx: &mut App) {
    #[cfg(target_os = "macos")]
    gpui::base::apply_system_reduce_motion(_cx);
}

const REFLOW: Duration = Duration::from_millis(200);

#[derive(Default)]
pub(crate) struct Generation<Key> {
    key: Option<Key>,
    since: Option<Instant>,
}

impl<Key: Copy + Eq> Generation<Key> {
    fn visible_sample(&mut self, key: Key, mut clock: Clock, cx: &mut App) -> (bool, Option<f32>) {
        if clock.measured && self.key.is_some_and(|old| old != key) {
            refresh_preference(cx);
            clock.animate = enabled(cx);
        }
        self.sample(key, clock)
    }

    pub fn sample(&mut self, key: Key, clock: Clock) -> (bool, Option<f32>) {
        let changed = clock.animate && self.key.is_some_and(|old| old != key);
        self.key = clock.measured.then_some(key);
        if !clock.animate {
            self.since = None;
        }
        if changed {
            self.since = Some(clock.now);
        }
        let progress = self.since.and_then(|since| {
            let elapsed = clock.now.saturating_duration_since(since);
            (elapsed < REFLOW).then(|| ease_out(elapsed.as_secs_f32() / REFLOW.as_secs_f32()))
        });
        if progress.is_none() {
            self.since = None;
        }
        (changed, progress)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Clock {
    now: Instant,
    measured: bool,
    animate: bool,
}

pub(crate) struct Viewport {
    measurement: Rc<Cell<Option<f32>>>,
    pub width: f32,
    pub clock: Clock,
}

impl Viewport {
    pub fn read(
        id: impl Into<ElementId>,
        fallback: f32,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let measurement = window
            .use_keyed_state(id, cx, |_, _| Rc::new(Cell::new(None::<f32>)))
            .read(cx)
            .clone();
        let measured = measurement.get();
        Self {
            measurement,
            width: measured.unwrap_or(fallback),
            clock: Clock {
                now: cx.background_executor().now(),
                measured: measured.is_some(),
                animate: measured.is_some() && enabled(cx),
            },
        }
    }

    pub fn observer<V: 'static>(self, cx: &Context<V>) -> impl IntoElement + use<V> {
        let entity = cx.entity().downgrade();
        canvas(
            move |bounds, window, cx| {
                let width = f32::from(bounds.size.width).floor();
                if width > 0. && self.measurement.replace(Some(width)) != Some(width) {
                    window.defer(cx, move |_, cx| {
                        let _ = entity.update(cx, |_, cx| cx.notify());
                    });
                }
            },
            |_, _, _, _| (),
        )
        .absolute()
        .inset_0()
    }
}

impl Clock {
    pub fn fade<Key: Copy + Eq + Default + 'static>(
        self,
        id: impl Into<ElementId>,
        key: Key,
        window: &mut Window,
        cx: &mut App,
    ) -> f32 {
        let generation = window.use_keyed_state(id, cx, |_, _| Generation::default());
        let (_, progress) = generation.update(cx, |state, cx| state.visible_sample(key, self, cx));
        progress.map_or(1., |progress| {
            window.request_animation_frame();
            mix(0.4, 1., progress)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Responsive;

    #[test]
    fn only_visible_breakpoint_changes_start_a_generation() {
        let mut generation = Generation::default();
        let now = Instant::now();
        let mut sample = |width, millis, measured, enabled| {
            generation.sample(
                Responsive::at(width).summary_columns,
                Clock {
                    now: now + Duration::from_millis(millis),
                    measured,
                    animate: measured && enabled,
                },
            )
        };
        assert_eq!(sample(1144., 0, false, true), (false, None));
        assert_eq!(sample(830., 10, true, true), (false, None));
        assert_eq!(sample(999., 20, true, true), (false, None));
        assert_eq!(sample(1000., 30, true, true), (true, Some(0.)));
        assert_eq!(sample(1100., 130, true, true), (false, Some(0.875)));
        assert_eq!(sample(1101., 230, true, true), (false, None));
        assert_eq!(sample(999., 240, true, true), (true, Some(0.)));
        assert_eq!(sample(999., 241, true, false), (false, None));
        assert_eq!(sample(1100., 242, true, false), (false, None));
    }
}
