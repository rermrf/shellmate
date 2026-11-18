//! The app icon (design scheme 1c, "本机与远端", from `docs/Shellmate 图标.html`).
//!
//! A packaged `Shellmate.app` carries `assets/app-icon/Shellmate.icns` (see
//! `scripts/bundle-macos.sh`). Setting the Dock icon at runtime as well covers a
//! bare `cargo run` binary, and lets the Dock follow the system light/dark
//! appearance with the dark variant, which a plain `.icns` cannot express.

use gpui::WindowAppearance;

pub(crate) const LIGHT: &[u8] = include_bytes!("../assets/app-icon/dock-light.png");
pub(crate) const DARK: &[u8] = include_bytes!("../assets/app-icon/dock-dark.png");

pub fn is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// Show the light or dark variant in the Dock and the app switcher.
/// Must run on the main thread; elsewhere it does nothing.
pub fn apply(dark: bool) {
    #[cfg(target_os = "macos")]
    {
        use objc2::{AllocAnyThread as _, MainThreadMarker};
        use objc2_app_kit::{NSApplication, NSImage};
        use objc2_foundation::NSData;

        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let data = NSData::with_bytes(if dark { DARK } else { LIGHT });
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
            log::warn!("app icon: could not decode the bundled PNG");
            return;
        };
        // SAFETY: called on the main thread with a valid, retained image.
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
    #[cfg(not(target_os = "macos"))]
    let _ = dark;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_icons_are_png_and_appearance_maps_to_variant() {
        for bytes in [LIGHT, DARK] {
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        }
        assert!(is_dark(WindowAppearance::Dark));
        assert!(is_dark(WindowAppearance::VibrantDark));
        assert!(!is_dark(WindowAppearance::Light));
        assert!(!is_dark(WindowAppearance::VibrantLight));
    }
}
