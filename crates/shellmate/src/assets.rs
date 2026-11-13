//! Asset source: gpui-kit's default component icons plus the extra Lucide
//! icons this app uses (the default `Assets` only embeds ~100 icons).

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

const EXTRA_ICONS: &[(&str, &[u8])] = &[
    ("icons/x.svg", include_bytes!("../assets/icons/x.svg")),
    ("icons/radio-tower.svg", include_bytes!("../assets/icons/radio-tower.svg")),
    ("icons/sparkles.svg", include_bytes!("../assets/icons/sparkles.svg")),
    ("icons/zap.svg", include_bytes!("../assets/icons/zap.svg")),
];

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match EXTRA_ICONS.iter().find(|(p, _)| *p == path) {
            Some((_, bytes)) => Ok(Some(Cow::Borrowed(bytes))),
            None => gpui::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut list = gpui::assets::Assets.list(path)?;
        list.extend(
            EXTRA_ICONS
                .iter()
                .filter(|(p, _)| p.starts_with(path))
                .map(|(p, _)| SharedString::new_static(p)),
        );
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::assets::IconName;

    /// Every Lucide icon referenced by the app must resolve, otherwise it
    /// silently renders as an empty box.
    #[test]
    fn all_used_icons_resolve() {
        let used = [
            IconName::PanelLeft,
            IconName::Plus,
            IconName::Search,
            IconName::SquareTerminal,
            IconName::ChevronDown,
            IconName::ChevronRight,
            IconName::ChevronUp,
            IconName::Star,
            IconName::Settings,
            IconName::X,
            IconName::RadioTower,
            IconName::Sparkles,
            IconName::Zap,
            IconName::Sun,
            IconName::Moon,
            IconName::Close,
        ];
        for icon in used {
            let path = icon.path();
            assert!(
                matches!(AppAssets.load(&path), Ok(Some(_))),
                "icon asset missing: {path}"
            );
        }
    }
}
