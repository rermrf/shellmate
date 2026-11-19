//! Finder, Dock drops and the debug hook share one ordered queue, including at cold launch.
use super::lifecycle::MainWindow;
use gpui::App;
use reqwest::Url;
use std::path::{Path, PathBuf};

const LIMIT: usize = 10;

pub(crate) fn listen(receiver: async_channel::Receiver<Vec<String>>, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Ok(urls) = receiver.recv().await {
            // Resolving a directory on a mounted volume can block.
            let dirs = cx
                .background_executor()
                .spawn(async move { directories(&urls) })
                .await;
            if !dirs.is_empty() {
                cx.update(|cx| {
                    MainWindow::with_restored_workspace(cx, move |workspace, window, cx| {
                        for dir in dirs {
                            log::info!("open url: local terminal in {}", dir.display());
                            workspace.open_local_in(Some(dir), window, cx);
                        }
                    })
                });
            }
        }
    })
    .detach();
}

#[cfg(debug_assertions)]
pub(crate) fn debug_hook(sender: async_channel::Sender<Vec<String>>, cx: &mut App) {
    if let Ok(value) = std::env::var("SHELLMATE_DEBUG_OPEN_URLS") {
        let timer = cx
            .background_executor()
            .timer(std::time::Duration::from_secs(3));
        cx.spawn(async move |_| {
            timer.await;
            let urls = value.split_whitespace().map(str::to_owned).collect();
            if let Err(err) = sender.send(urls).await {
                log::warn!("open url: workspace unavailable: {err}");
            }
        })
        .detach();
    }
}

fn directories(urls: &[String]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for raw in urls {
        let Ok(url) = Url::parse(raw) else {
            log::warn!("open url: malformed URL {raw:?}");
            continue;
        };
        match url.scheme() {
            "shellmate"
                if url.authority().eq_ignore_ascii_case("open")
                    && url.path().is_empty()
                    && url.fragment().is_none() =>
            {
                let mut has_path = false;
                for (name, value) in url.query_pairs() {
                    if name == "path" {
                        has_path = true;
                        add_directory(Path::new(value.as_ref()), &mut dirs);
                    } else {
                        log::warn!("open url: unknown parameter {name:?}");
                    }
                }
                if !has_path {
                    log::warn!("open url: missing path parameter");
                }
            }
            // The URL parser also expands relative file: URLs. Require a local absolute URL
            // in the original input before asking it to percent-decode the filesystem path.
            "file"
                if raw
                    .split_once(':')
                    .is_some_and(|(_, rest)| rest.starts_with("///"))
                    && url.host().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none() =>
            {
                match url.to_file_path() {
                    Ok(path) => add_directory(&path, &mut dirs),
                    Err(()) => log::warn!("open url: invalid local file URL {raw:?}"),
                }
            }
            _ => log::warn!("open url: unsupported URL {raw:?}"),
        }
    }
    dirs
}

fn add_directory(path: &Path, dirs: &mut Vec<PathBuf>) {
    if dirs.len() == LIMIT {
        log::warn!("open url: skipping {path:?}, limit of {LIMIT} directories reached");
        return;
    }
    if !path.is_absolute() {
        log::warn!("open url: expected an absolute path, got {path:?}");
        return;
    }
    let resolved = path.canonicalize().and_then(|mut dir| {
        let metadata = dir.metadata()?;
        if metadata.is_file() {
            dir.pop();
        } else if !metadata.is_dir() {
            return Err(std::io::Error::other(
                "expected a directory or regular file",
            ));
        }
        Ok(dir)
    });
    match resolved {
        Ok(dir) if dirs.contains(&dir) => log::warn!("open url: skipping duplicate {dir:?}"),
        Ok(dir) => dirs.push(dir),
        Err(err) => log::warn!("open url: skipping {path:?}: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finder_selection_decodes_paths_maps_files_and_preserves_first_seen_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let folder = root.join("空 格+&=%#");
        fs::create_dir(&folder).unwrap();
        fs::write(root.join("selected.txt"), []).unwrap();
        std::os::unix::fs::symlink(&folder, root.join("alias")).unwrap();
        let encoded = format!(
            "shellmate://open?path={0}/%E7%A9%BA%20%E6%A0%BC%2B%26%3D%25%23&path={0}/selected.txt&path={0}/alias",
            root.display()
        );
        assert_eq!(
            directories(&[encoded]),
            [folder.canonicalize().unwrap(), root.canonicalize().unwrap()]
        );
    }

    #[test]
    fn skips_relative_missing_and_special_files_and_unknown_parameters() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::os::unix::fs::symlink("/dev/null", root.join("device")).unwrap();
        fs::create_dir(root.join("other")).unwrap();
        let url = format!(
            "shellmate://open?path=relative&path={0}/missing&path={0}/device&cmd={0}/other&path={0}",
            root.display()
        );
        assert_eq!(directories(&[url]), [root.canonicalize().unwrap()]);
    }

    #[test]
    fn rejects_other_schemes_authorities_routes_and_file_parameters() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().display();
        for authority in [
            "https://open",
            "shellmate://other",
            "shellmate://user@open",
            "shellmate://open:80",
            "shellmate://open/run",
        ] {
            assert!(directories(&[format!("{authority}?path={root}")]).is_empty());
        }
        for url in [
            format!("file://remote{root}"),
            format!("file://{root}?run=ls"),
            format!("shellmate://open?path={root}#fragment"),
            "file:relative".into(),
        ] {
            assert!(directories(&[url]).is_empty());
        }
    }

    #[test]
    fn dock_file_urls_decode_paths_and_map_files_to_their_directory() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("资料 folder");
        fs::create_dir(&folder).unwrap();
        let file = folder.join("notes + %.txt");
        fs::write(&file, []).unwrap();
        for path in [&folder, &file] {
            let url = Url::from_file_path(path).unwrap();
            assert_eq!(directories(&[url.into()]), [folder.canonicalize().unwrap()]);
        }
    }

    #[test]
    fn caps_the_whole_delivery_at_ten_distinct_directories() {
        let temp = tempfile::tempdir().unwrap();
        let mut url = Url::parse("shellmate://open").unwrap();
        let mut expected = Vec::new();
        for index in 0..11 {
            let folder = temp.path().join(index.to_string());
            fs::create_dir(&folder).unwrap();
            url.query_pairs_mut()
                .append_pair("path", folder.to_str().unwrap());
            if index < 10 {
                expected.push(folder.canonicalize().unwrap());
            }
        }
        let drop_url = Url::from_file_path(temp.path()).unwrap();
        assert_eq!(directories(&[url.into(), drop_url.into()]), expected);
    }
}
