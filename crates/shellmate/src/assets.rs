//! Prototype icons, plus gpui-kit and supplemental Lucide icons for controls absent from the prototype.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

const EXTRA_ICONS: &[(&str, &[u8])] = &[
    ("dock-light.png", crate::app_icon::LIGHT),
    ("dock-dark.png", crate::app_icon::DARK),
    ("icons/x.svg", include_bytes!("../assets/icons/x.svg")),
    ("icons/radio-tower.svg", include_bytes!("../assets/icons/radio-tower.svg")),
    ("icons/sparkles.svg", include_bytes!("../assets/icons/sparkles.svg")),
    ("icons/zap.svg", include_bytes!("../assets/icons/zap.svg")),
    ("icons/ellipsis.svg", include_bytes!("../assets/icons/ellipsis.svg")),
    ("icons/pencil.svg", include_bytes!("../assets/icons/pencil.svg")),
    ("icons/copy.svg", include_bytes!("../assets/icons/copy.svg")),
    ("icons/trash.svg", include_bytes!("../assets/icons/trash.svg")),
    ("icons/play.svg", include_bytes!("../assets/icons/play.svg")),
    ("icons/folder-open.svg", include_bytes!("../assets/icons/folder-open.svg")),
    ("icons/arrow-up.svg", include_bytes!("../assets/icons/arrow-up.svg")),
    ("icons/link.svg", include_bytes!("../assets/icons/link.svg")),
];

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = crate::icons::ASSETS.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        let asset = match EXTRA_ICONS.iter().find(|(p, _)| *p == path) {
            Some((_, bytes)) => Some(Cow::Borrowed(*bytes)),
            None => gpui::assets::Assets.load(path)?,
        };
        // Lucide sources use a 24-unit viewBox; 2.4 units yields 1.6px at 16px display size.
        Ok(asset.map(|asset| {
            if path.starts_with("icons/")
                && path.ends_with(".svg")
                && String::from_utf8_lossy(&asset).contains("viewBox=\"0 0 24 24\"")
            {
                Cow::Owned(
                    String::from_utf8_lossy(&asset).replace("stroke-width=\"2\"", "stroke-width=\"2.4\"").into_bytes(),
                )
            } else {
                asset
            }
        }))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut list = gpui::assets::Assets.list(path)?;
        list.extend(EXTRA_ICONS.iter().filter(|(p, _)| p.starts_with(path)).map(|(p, _)| SharedString::new_static(p)));
        list.extend(
            crate::icons::ASSETS
                .iter()
                .filter(|(name, _)| name.starts_with(path))
                .map(|(name, _)| SharedString::new_static(name)),
        );
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::assets::IconName;

    #[test]
    fn prototype_icons_resolve_without_modifying_the_original_markup() {
        assert_eq!(crate::icons::AppIcon::ALL.len(), 19);
        for icon in crate::icons::AppIcon::ALL {
            let path = icon.path();
            let original =
                crate::icons::ASSETS.iter().find(|(name, _)| *name == path).expect("registered prototype asset").1;
            let loaded = AppAssets.load(path).unwrap().expect("prototype asset exists");
            assert_eq!(loaded.as_ref(), original, "custom stroke widths must not be rewritten: {path}");
            let svg = std::str::from_utf8(original).unwrap();
            assert!(svg.starts_with("<svg ") && svg.contains("viewBox=") && svg.ends_with("</svg>\n"));
            assert!(!svg.contains("var(--") && !svg.contains("sc-camel-"), "asset uses native SVG attributes");
            assert!(AppAssets.list("prototype/").unwrap().iter().any(|name| name.as_ref() == path));
        }
        let gear = AppAssets.load(crate::icons::AppIcon::Settings.path()).unwrap().unwrap();
        let gear = std::str::from_utf8(&gear).unwrap();
        assert!(gear.contains("viewBox=\"0 0 24 24\"") && gear.contains("stroke-width=\"1.8\""));
    }

    #[test]
    fn prototype_svgs_parse_and_rasterize_without_a_window() {
        let renderer = gpui::SvgRenderer::new(std::sync::Arc::new(AppAssets));
        for icon in crate::icons::AppIcon::ALL {
            let bytes = AppAssets.load(icon.path()).unwrap().unwrap();
            let svg = renderer.parse_svg(&bytes).unwrap_or_else(|e| panic!("{}: {e}", icon.path()));
            let size = gpui::size(
                gpui::DevicePixels((icon.size() * 2.) as i32),
                gpui::DevicePixels((icon.size() * 2.) as i32),
            );
            let image = renderer.render_parsed(&svg, gpui::SvgSize::ExactSize(size)).unwrap();
            assert_eq!(image.size(0), size);
            assert!(
                image.as_bytes(0).unwrap().as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0),
                "{} must draw visible ink",
                icon.path()
            );
        }
    }

    #[test]
    fn supplemental_lucide_strokes_keep_their_optical_weight() {
        let svg = AppAssets.load("icons/copy.svg").unwrap().unwrap();
        assert!(std::str::from_utf8(&svg).unwrap().contains("stroke-width=\"2.4\""));
    }

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
            IconName::ArrowLeft,
            IconName::ArrowRight,
            IconName::Link,
            IconName::Folder,
            IconName::File,
            IconName::RefreshCw,
            IconName::TriangleAlert,
            IconName::CircleAlert,
        ];
        for icon in used {
            let path = icon.path();
            assert!(matches!(AppAssets.load(&path), Ok(Some(_))), "icon asset missing: {path}");
        }
    }
}
