//! Shellmate's prototype SVGs. Native 16-unit geometry is never Lucide-normalized.
//! The template's gear is the exception: its own 24-unit viewBox and 1.8 stroke are preserved.

use crate::theme::Palette;
use gpui::component::Icon;
use gpui::{IntoElement, div, prelude::*, px};

macro_rules! prototype_icons {
    ($( $variant:ident => ($file:literal, $size:literal) ),+ $(,)?) => {
        #[derive(Clone, Copy)]
        pub enum AppIcon { $( $variant ),+ }
        impl AppIcon {
            pub fn path(self) -> &'static str { match self { $( Self::$variant => concat!("prototype/",$file,".svg") ),+ } }
            pub fn size(self) -> f32 { match self { $( Self::$variant => $size ),+ } }
            pub fn icon(self) -> Icon { self.at(self.size()) }
            pub fn at(self, size: f32) -> Icon { Icon::empty().path(self.path()).size(px(size)) }
            #[cfg(test)]
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
        pub const ASSETS: &[(&str, &[u8])] = &[
            $( (concat!("prototype/",$file,".svg"),include_bytes!(concat!("../assets/prototype/",$file,".svg"))) ),+
        ];
    }
}
prototype_icons! {
    Sidebar => ("sidebar", 16.),
    Search => ("search", 15.),
    Plus => ("plus", 16.),
    LocalTerminal => ("local-terminal", 17.),
    Laptop => ("laptop", 15.),
    Settings => ("settings", 16.),
    Broadcast => ("broadcast", 15.),
    Ai => ("ai", 14.),
    Commands => ("commands", 14.),
    Moon => ("moon", 15.),
    Sun => ("sun", 16.),
    Folder => ("folder", 17.),
    FolderFill => ("folder-fill", 17.),
    File => ("file", 17.),
    GroupChevron => ("group-chevron", 12.),
}

/// GPUI SVGs are alpha masks. Separate layers retain the prototype's independent fill/stroke colors.
pub fn folder(p: &Palette) -> impl IntoElement {
    div()
        .relative()
        .size(px(17.))
        .flex_none()
        .child(AppIcon::FolderFill.icon().text_color(p.accs).absolute().inset_0())
        .child(AppIcon::Folder.icon().text_color(p.acc).absolute().inset_0())
}

/// The literal system-font › contour, quarter-turned exactly as the prototype's CSS span.
pub fn group_chevron(open: bool) -> Icon {
    Icon::empty()
        .path(AppIcon::GroupChevron.path())
        .w(px(10.))
        .h(px(15.))
        .when(open, |icon| icon.rotate(gpui::radians(std::f32::consts::FRAC_PI_2)))
}
