//! Asset source: gpui-kit's default component icons plus the extra Lucide
//! icons this app uses (the default `Assets` only embeds ~100 icons).

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

const EXTRA_ICONS: &[(&str, &[u8])] = &[
    ("icons/x.svg", include_bytes!("../assets/icons/x.svg")),
    (
        "icons/radio-tower.svg",
        include_bytes!("../assets/icons/radio-tower.svg"),
    ),
    (
        "icons/sparkles.svg",
        include_bytes!("../assets/icons/sparkles.svg"),
    ),
    ("icons/zap.svg", include_bytes!("../assets/icons/zap.svg")),
    (
        "icons/ellipsis.svg",
        include_bytes!("../assets/icons/ellipsis.svg"),
    ),
    ("icons/pencil.svg", include_bytes!("../assets/icons/pencil.svg")),
    ("icons/copy.svg", include_bytes!("../assets/icons/copy.svg")),
    ("icons/trash.svg", include_bytes!("../assets/icons/trash.svg")),
    ("icons/play.svg", include_bytes!("../assets/icons/play.svg")),
    (
        "icons/folder-open.svg",
        include_bytes!("../assets/icons/folder-open.svg"),
    ),
    (
        "icons/arrow-up.svg",
        include_bytes!("../assets/icons/arrow-up.svg"),
    ),
    ("icons/link.svg", include_bytes!("../assets/icons/link.svg")),
];

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let asset = match EXTRA_ICONS.iter().find(|(p, _)| *p == path) {
            Some((_, bytes)) => Some(Cow::Borrowed(*bytes)),
            None => gpui::assets::Assets.load(path)?,
        };
        // Lucide sources use a 24-unit viewBox; 2.4 units yields 1.6px at 16px display size.
        Ok(asset.map(|asset| {
            if path.starts_with("icons/") && path.ends_with(".svg") {
                Cow::Owned(
                    String::from_utf8_lossy(&asset)
                        .replace("stroke-width=\"2\"", "stroke-width=\"2.4\"")
                        .into_bytes(),
                )
            } else {
                asset
            }
        }))
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
            IconName::Ellipsis,
            IconName::Pencil,
            IconName::Copy,
            IconName::Trash,
            IconName::Play,
            IconName::FolderOpen,
            IconName::ArrowUp,
            IconName::ArrowDown,
            IconName::Link,
            IconName::Folder,
            IconName::File,
            IconName::RefreshCw,
            IconName::TriangleAlert,
            IconName::CircleAlert,
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
