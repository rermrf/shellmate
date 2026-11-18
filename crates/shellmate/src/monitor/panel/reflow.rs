use crate::layout::Responsive;
use crate::motion::blocks::{Layout, Rect};

pub(super) fn summary(width: f32, columns: usize) -> Layout<4> {
    let cell = ((width - 10. * (columns - 1) as f32) / columns as f32).max(0.);
    Layout {
        rects: std::array::from_fn(|index| {
            Rect::new(
                (index % columns) as f32 * (cell + 10.),
                (index / columns) as f32 * 167.,
                cell,
                157.,
            )
        }),
        height: if columns == 4 { 157. } else { 324. },
    }
}

pub(super) fn lower_widths(width: f32, stacked: bool) -> [f32; 2] {
    if stacked {
        return [width; 2];
    }
    let available = (width - 10.).max(0.);
    let cores = available / 2.35;
    [available - cores, cores]
}

pub(super) fn lower(width: f32, stacked: bool, heights: [f32; 2], viewport: f32) -> Layout<2> {
    let [attention, cores] = lower_widths(width, stacked);
    if stacked {
        Layout {
            rects: [
                Rect::new(0., 0., attention, heights[0]),
                Rect::new(0., heights[0] + 10., cores, heights[1]),
            ],
            height: heights[0] + 10. + heights[1],
        }
    } else {
        let height = heights[0]
            .max(heights[1])
            .max(viewport - summary(width, 4).height - 10.);
        Layout {
            rects: [
                Rect::new(0., 0., attention, height),
                Rect::new(attention + 10., 0., cores, height),
            ],
            height,
        }
    }
}

pub(super) fn header_widths(width: f32, stacked: bool, tabs: f32) -> [f32; 2] {
    let inner = (width - 28.).max(0.);
    let tabs = tabs.min(inner);
    [
        if stacked {
            inner
        } else {
            (inner - tabs - 14.).max(0.)
        },
        tabs,
    ]
}

pub(super) fn header(widths: [f32; 2], stacked: bool, info_height: f32) -> Layout<2> {
    let [info, tabs] = widths;
    if stacked {
        Layout {
            rects: [
                Rect::new(18., 10., info, info_height),
                Rect::new(18., 24. + info_height, tabs, 38.),
            ],
            height: 72. + info_height,
        }
    } else {
        let height = (info_height.max(38.) + 20.).max(64.);
        Layout {
            rects: [
                Rect::new(18., (height - info_height) / 2., info, info_height),
                Rect::new(32. + info, (height - 38.) / 2., tabs, 38.),
            ],
            height,
        }
    }
}

pub(super) fn endpoint(width: f32, frozen: bool, source: bool) -> (f32, Responsive) {
    let width = if !frozen {
        width
    } else if source {
        width.max(1000.)
    } else {
        width.min(999.)
    };
    (width, Responsive::at(width))
}
