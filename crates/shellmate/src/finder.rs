//! Finder Sync preferences belong to the containing app, and are managed by macOS.
use std::path::Path;

#[link(name = "FinderSync", kind = "framework")]
unsafe extern "C" {}

fn controller() -> &'static objc2::runtime::AnyClass {
    objc2::runtime::AnyClass::get(c"FIFinderSyncController").expect("linked FinderSync framework")
}

fn bundled_extension(executable: &Path) -> bool {
    executable.ancestors().any(|bundle| {
        bundle.extension().is_some_and(|ext| ext == "app")
            && bundle
                .join("Contents/PlugIns/ShellmateFinder.appex")
                .is_dir()
    })
}

/// None when running a standalone development binary.
pub(crate) fn enabled() -> Option<bool> {
    let exe = std::env::current_exe().ok()?;
    bundled_extension(&exe).then(|| {
        // The SDK declares +isExtensionEnabled as a BOOL class method.
        unsafe { objc2::msg_send![controller(), isExtensionEnabled] }
    })
}

pub(crate) fn show_settings() {
    // Called from the settings button on the main thread. The class method returns void.
    unsafe { objc2::msg_send![controller(), showExtensionManagementInterface] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_an_app_with_the_embedded_extension() {
        let temp = tempfile::tempdir().unwrap();
        let contents = temp.path().join("Shellmate.app/Contents");
        let exe = contents.join("MacOS/Shellmate");
        assert!(!bundled_extension(&exe));
        std::fs::create_dir_all(contents.join("PlugIns/ShellmateFinder.appex")).unwrap();
        assert!(bundled_extension(&exe));
        assert!(!bundled_extension(&temp.path().join("shellmate")));
    }
}
