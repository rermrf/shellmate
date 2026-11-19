use super::*;
use crate::motion::{CHROME_DURATION, ease_out, mix};
use std::time::Instant;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Drawer {
    #[default]
    Closed,
    Ai,
    Commands,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Frame {
    ai_share: f32,
    commands_share: f32,
    ai_alpha: f32,
    commands_alpha: f32,
}

impl Frame {
    fn endpoint(drawer: Drawer) -> Self {
        let ai = u8::from(drawer == Drawer::Ai) as f32;
        let commands = u8::from(drawer == Drawer::Commands) as f32;
        Self {
            ai_share: ai,
            commands_share: commands,
            ai_alpha: ai,
            commands_alpha: commands,
        }
    }

    fn sample(self, target: Self, time: f32) -> Self {
        let progress = ease_out(time);
        // On a swap the outgoing drawer fades over the first 40% and the incoming one over the rest,
        // so the two never show their text on top of each other. A lone close fades out early,
        // before the slot has closed; a lone open fades in over the whole flight.
        let swap = (target.ai_alpha > self.ai_alpha || target.commands_alpha > self.commands_alpha)
            && (target.ai_alpha < self.ai_alpha || target.commands_alpha < self.commands_alpha);
        let fade = |from: f32, to: f32| {
            let phase = if to > from {
                if swap { ((time - 0.4) / 0.6).clamp(0., 1.) } else { time }
            } else if swap {
                (time / 0.4).clamp(0., 1.)
            } else {
                (time / 0.6).clamp(0., 1.)
            };
            mix(from, to, phase)
        };
        Self {
            ai_share: mix(self.ai_share, target.ai_share, progress),
            commands_share: mix(self.commands_share, target.commands_share, progress),
            ai_alpha: fade(self.ai_alpha, target.ai_alpha),
            commands_alpha: fade(self.commands_alpha, target.commands_alpha),
        }
    }

    fn width(self, ai_width: f32) -> f32 {
        self.ai_share * ai_width + self.commands_share * crate::productivity::panel::DRAWER_WIDTH
    }

    fn gap(self) -> f32 {
        12. * (self.ai_share + self.commands_share)
    }

    #[cfg(debug_assertions)]
    fn occupied(self, ai_width: f32) -> f32 {
        self.width(ai_width) + self.gap()
    }
}

#[derive(Default)]
pub(super) struct DrawerMotion {
    target: Option<Drawer>,
    frame: Frame,
    flight: Option<(Frame, Instant)>,
    #[cfg(debug_assertions)]
    frozen_from: Option<Drawer>,
}

impl DrawerMotion {
    pub(super) fn changing_target(&self, target: Drawer) -> bool {
        self.target != Some(target)
    }

    pub(super) fn update(&mut self, target: Drawer, now: Instant, animate: bool) {
        #[cfg(debug_assertions)]
        if self.frozen_from.is_some() {
            return;
        }
        let end = Frame::endpoint(target);
        if !animate || self.target.is_none() {
            self.flight = None;
            self.frame = end;
        } else if self.target != Some(target) {
            self.flight = (self.frame != end).then_some((self.frame, now));
        }
        self.target = Some(target);
        if let Some((start, since)) = self.flight {
            let elapsed = now.saturating_duration_since(since);
            if elapsed >= CHROME_DURATION {
                self.frame = end;
                self.flight = None;
            } else {
                self.frame =
                    start.sample(end, elapsed.as_secs_f32() / CHROME_DURATION.as_secs_f32());
            }
        }
    }

    pub(super) fn running(&self) -> bool {
        self.flight.is_some()
    }

    #[cfg(debug_assertions)]
    pub(super) fn freeze(&mut self, from: Drawer, to: Drawer) {
        self.target = Some(to);
        self.frozen_from = Some(from);
        self.frame = Frame::endpoint(from).sample(Frame::endpoint(to), 0.5);
    }

    #[cfg(debug_assertions)]
    pub(super) fn grid_offset(&self, ai_width: f32) -> f32 {
        self.frozen_from.map_or(0., |from| {
            self.frame.occupied(ai_width) - Frame::endpoint(from).occupied(ai_width)
        })
    }
}

impl Workspace {
    pub(super) fn drawer_kind(&self) -> Drawer {
        if self.ai_open {
            Drawer::Ai
        } else if self.commands_open {
            Drawer::Commands
        } else {
            Drawer::Closed
        }
    }

    pub(super) fn drawer_slot(&self, ai_width: f32, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let frame = self.drawer_motion.frame;
        let width = frame.width(ai_width);
        if frame.gap() == 0. {
            return None;
        }
        let moving = frame != Frame::endpoint(self.drawer_kind());
        let slot = div()
            .relative()
            .ml(px(frame.gap()))
            .w(px(width))
            .flex_none()
            .h_full()
            .min_h_0();
        if !moving {
            return Some(if self.ai_open {
                slot.children(self.ai_drawer.clone())
                    .child(self.drawer_handle(cx))
            } else {
                slot.child(self.commands.clone())
            });
        }
        let layer = |natural_width, alpha| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(18. * (1. - frame.ai_share - frame.commands_share)))
                .w(px(natural_width))
                .opacity(alpha)
        };
        let ai = (frame.ai_alpha > 0.)
            .then(|| layer(ai_width, frame.ai_alpha).children(self.ai_drawer.clone()));
        let commands = (frame.commands_alpha > 0.).then(|| {
            layer(
                crate::productivity::panel::DRAWER_WIDTH,
                frame.commands_alpha,
            )
            .child(self.commands.clone())
        });
        Some(
            slot.overflow_hidden()
                .children(ai)
                .children(commands)
                .child(div().id("drawer-in-flight").absolute().inset_0().occlude()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_close_and_swap_sample_one_slot() {
        let ai = Frame::endpoint(Drawer::Ai);
        let commands = Frame::endpoint(Drawer::Commands);
        let closed = Frame::endpoint(Drawer::Closed);
        for (start, end) in [
            (closed, ai),
            (ai, closed),
            (closed, commands),
            (commands, closed),
            (ai, commands),
            (commands, ai),
        ] {
            assert_eq!(start.sample(end, 0.), start);
            assert_eq!(start.sample(end, 1.), end);
            let mid = start.sample(end, 0.5);
            assert_eq!(
                mid.width(560.),
                mix(start.width(560.), end.width(560.), 0.875)
            );
            assert_eq!(mid.gap(), mix(start.gap(), end.gap(), 0.875));
        }
        // A swap never shows both drawers' text at once: out by 40%, in from 40%.
        for time in [0.1, 0.3, 0.4, 0.5, 0.7, 0.9] {
            let frame = ai.sample(commands, time);
            assert!(frame.ai_alpha == 0. || frame.commands_alpha == 0., "{time}: {frame:?}");
            assert_eq!(frame.gap(), 12.);
        }
        let mid = ai.sample(commands, 0.7);
        assert!(mid.ai_alpha == 0. && (mid.commands_alpha - 0.5).abs() < 1e-5, "{mid:?}");
        // A lone close is invisible before the slot is shut.
        assert_eq!(ai.sample(closed, 0.6).ai_alpha, 0.);
    }

    #[test]
    fn drawer_retargets_from_last_displayed_frame() {
        let mut motion = DrawerMotion::default();
        let start = Instant::now();
        motion.update(Drawer::Closed, start, true);
        for (step, goal) in [Drawer::Ai, Drawer::Commands, Drawer::Ai, Drawer::Closed]
            .into_iter()
            .enumerate()
        {
            let now = start + CHROME_DURATION * step as u32 / 3;
            let displayed = motion.frame;
            motion.update(goal, now, true);
            assert_eq!(motion.frame, displayed);
            motion.update(goal, now + CHROME_DURATION / 4, true);
            assert_ne!(motion.frame, displayed);
        }
        motion.update(Drawer::Closed, start + CHROME_DURATION * 3, true);
        assert!(!motion.running());
        assert_eq!(motion.frame, Frame::endpoint(Drawer::Closed));
    }

    #[test]
    fn restore_reduce_motion_and_drag_use_settled_width() {
        let now = Instant::now();
        let mut motion = DrawerMotion::default();
        motion.update(Drawer::Ai, now, true);
        assert!(!motion.running());
        assert_eq!(motion.frame.width(425.), 425.);
        assert_eq!(motion.frame.width(510.), 510.);
        motion.update(Drawer::Commands, now, true);
        assert!(motion.running());
        motion.update(Drawer::Commands, now, false);
        assert!(!motion.running());
        assert_eq!(motion.frame, Frame::endpoint(Drawer::Commands));
    }
}
