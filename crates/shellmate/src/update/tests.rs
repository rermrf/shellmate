use super::{
    check::{self, Cache, Outcome},
    download, install,
    release::{Asset, Checksum, Release, checksum_file},
};
use reqwest::Client;
use semver::Version;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::oneshot,
};

const NAME: &str = "Shellmate-0.1.9-arm64.dmg";
const UNIVERSAL: &str = "Shellmate-0.1.9-universal.dmg";
const DATA: &[u8] = b"fixture disk image";

fn hash(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn fixture(api: &str, bytes: &[u8]) -> Value {
    let asset = |name: &str| {
        json!({"name":name,"size":bytes.len(),"digest":format!("sha256:{}",hex::encode(hash(bytes))),
        "browser_download_url":format!("{api}/{name}")})
    };
    json!({"tag_name":"v0.1.9","name":"Shellmate 0.1.9","body":"- 测试发布","html_url":format!("{api}/page"),
    "published_at":"2026-11-20T10:20:00Z","assets":[asset(NAME),asset(UNIVERSAL),{
        "name":format!("{NAME}.sha256"),"size":100,"digest":null,"browser_download_url":format!("{api}/sum")
    }]})
}

#[test]
fn github_release_versions_and_assets() {
    let mut value = fixture("http://127.0.0.1", DATA);
    let r = Release::parse(&value.to_string()).unwrap();
    assert_eq!(
        (r.name.as_str(), r.body.as_str()),
        ("Shellmate 0.1.9", "- 测试发布")
    );
    assert_eq!(r.page, "http://127.0.0.1/page");
    assert_eq!(r.published, "2026-11-20T10:20:00Z");
    assert_eq!(r.asset(true).unwrap().name, NAME);
    assert_eq!(r.asset(false).unwrap().name, UNIVERSAL);
    value["assets"].as_array_mut().unwrap().remove(0);
    let r = Release::parse(&value.to_string()).unwrap();
    assert_eq!(r.asset(true).unwrap().name, UNIVERSAL);
    value["assets"].as_array_mut().unwrap().remove(0);
    assert!(
        Release::parse(&value.to_string())
            .unwrap()
            .asset(false)
            .is_none()
    );
    let current = Version::parse("0.1.8").unwrap();
    for (tag, newer) in [
        ("v0.1.9", true),
        ("0.1.8", false),
        ("v0.1.7", false),
        ("v1.0.0-rc.1", false),
        ("v0.1.8+build.2", false),
    ] {
        value["tag_name"] = tag.into();
        assert_eq!(
            Release::parse(&value.to_string())
                .unwrap()
                .newer_than(&current),
            newer
        );
    }
    value["tag_name"] = "vbroken".into();
    assert!(Release::parse(&value.to_string()).is_err());
}

#[test]
fn digest_precedes_named_checksum_and_invalid_values_fail_closed() {
    let mut v = fixture("http://127.0.0.1", DATA);
    let parse = |v: &Value| {
        let r = Release::parse(&v.to_string()).unwrap();
        r.checksum(r.asset(true).unwrap())
    };
    assert!(matches!(parse(&v).unwrap(),Checksum::Digest(h) if h==hash(DATA)));
    for bad in ["sha256:0102", "sha1:abcd"] {
        v["assets"][0]["digest"] = bad.into();
        assert!(parse(&v).is_err());
    }
    v["assets"][0]["digest"] = Value::Null;
    assert!(matches!(parse(&v).unwrap(),Checksum::File(url) if url.ends_with("/sum")));
    v["assets"].as_array_mut().unwrap().pop();
    assert!(parse(&v).is_err());
    let text = format!("{}  {NAME}\n", hex::encode_upper(hash(DATA)));
    assert_eq!(checksum_file(&text, NAME).unwrap(), hash(DATA));
    assert!(checksum_file(&text, UNIVERSAL).is_err());
}

// Each response serves exactly one request. Tests can assert the precise URL and headers,
// or leave a body unfinished to exercise cancellation while a reader is blocked.
struct Reply {
    status: &'static str,
    headers: &'static str,
    data: Vec<u8>,
    stall: bool,
}
impl Reply {
    fn ok(data: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "200 OK",
            headers: "",
            data: data.into(),
            stall: false,
        }
    }
}

struct Mock {
    listener: TcpListener,
    api: String,
}
impl Mock {
    async fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("loopback listener");
        let api = format!("http://{}", listener.local_addr().unwrap());
        Self { listener, api }
    }

    fn serve(self, replies: Vec<Reply>) -> tokio::task::JoinHandle<Vec<String>> {
        tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (socket, _) =
                    tokio::time::timeout(Duration::from_secs(5), self.listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut socket = BufReader::new(socket);
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                requests.push(request);
                let len = reply.data.len() + usize::from(reply.stall);
                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {len}\r\nConnection: close\r\n{}\r\n",
                    reply.status, reply.headers
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&reply.data).await.unwrap();
                if reply.stall {
                    let mut line = String::new();
                    // The cancelled client closes the unfinished response.
                    let _closed_or_reset =
                        tokio::time::timeout(Duration::from_secs(5), socket.read_line(&mut line))
                            .await
                            .expect("the client should close the cancelled response");
                }
            }
            requests
        })
    }
}

#[tokio::test]
async fn conditional_requests_and_rate_limits() {
    let mock = Mock::new().await;
    let api = mock.api.clone();
    let server = mock.serve(vec![
        Reply {
            headers: "ETag: \"fixture\"\r\n",
            ..Reply::ok(fixture(&api, DATA).to_string())
        },
        Reply {
            status: "304 Not Modified",
            ..Reply::ok("")
        },
        Reply {
            status: "403 Forbidden",
            headers: "X-RateLimit-Remaining: 0\r\n",
            ..Reply::ok("")
        },
        Reply {
            status: "429 Too Many Requests",
            headers: "Retry-After: 30\r\n",
            ..Reply::ok("")
        },
        Reply {
            status: "403 Forbidden",
            ..Reply::ok("")
        },
    ]);
    let client = check::client().unwrap();
    let Outcome::Release(r, cache) = check::latest(&client, &api, &Cache::default())
        .await
        .unwrap()
    else {
        panic!("expected release")
    };
    assert_eq!(r.version.to_string(), "0.1.9");
    assert_eq!(cache.etag.as_deref(), Some("\"fixture\""));
    assert!(matches!(
        check::latest(&client, &api, &cache).await.unwrap(),
        Outcome::Same
    ));
    assert_eq!(Release::parse(&cache.json).unwrap().version, r.version);
    for _ in 0..2 {
        assert!(matches!(
            check::latest(&client, &api, &cache).await.unwrap(),
            Outcome::RateLimited
        ));
    }
    assert!(check::latest(&client, &api, &cache).await.is_err());
    let requests = server.await.unwrap();
    let headers = requests[0].to_ascii_lowercase();
    assert!(headers.starts_with("get /repos/rermrf/shellmate/releases/latest http/1.1\r\n"));
    for header in [
        "accept: application/vnd.github+json",
        "x-github-api-version: 2022-11-28",
        concat!("user-agent: shellmate/", env!("CARGO_PKG_VERSION")),
    ] {
        assert!(headers.contains(header));
    }
    assert!(!headers.contains("if-none-match"));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("if-none-match: \"fixture\"")
    );
}

fn asset(api: &str) -> Asset {
    Asset {
        name: NAME.into(),
        size: DATA.len() as u64,
        url: format!("{api}/{NAME}"),
        digest: None,
    }
}
async fn fetch(
    client: &Client,
    asset: &Asset,
    source: Checksum,
    dir: &Path,
    progress: impl FnMut(u64, u64),
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let (_tx, rx) = oneshot::channel();
    download::download(client, asset, source, dir, rx, progress).await
}

#[tokio::test]
async fn download_with_checksum_file_progress_reuse_and_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::new().await;
    let asset = asset(&mock.api);
    let sum = Checksum::File(format!("{}/sum", mock.api));
    let full_hash = hash(DATA);
    let server = mock.serve(vec![
        Reply::ok(format!("{}  {NAME}\n", hex::encode(full_hash))),
        Reply::ok(DATA),
    ]);
    let client = check::client().unwrap();
    let mut events = vec![];
    let path = fetch(&client, &asset, sum, dir.path(), |n, total| {
        events.push((n, total))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), DATA);
    assert_eq!(events.first(), Some(&(0, asset.size)));
    assert_eq!(events.last(), Some(&(asset.size, asset.size)));
    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("GET /sum "));
    assert!(requests[1].starts_with(&format!("GET /{NAME} ")));
    // The server has gone away. A verified cached file still completes.
    assert_eq!(
        fetch(
            &client,
            &asset,
            Checksum::Digest(full_hash),
            dir.path(),
            |_, _| {}
        )
        .await
        .unwrap()
        .unwrap(),
        path
    );
    fs::write(&path, vec![0; DATA.len()]).unwrap();
    let mock = Mock::new().await;
    let asset = Asset {
        url: mock.api.clone(),
        ..asset
    };
    let server = mock.serve(vec![Reply::ok(DATA)]);
    let err = fetch(
        &client,
        &asset,
        Checksum::Digest([42; 32]),
        dir.path(),
        |_, _| {},
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("SHA-256"));
    assert!(!dir.path().join(format!("{NAME}.part")).exists());
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_stalled_download_removes_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::new().await;
    let asset = asset(&mock.api);
    let server = mock.serve(vec![Reply {
        stall: true,
        ..Reply::ok(&DATA[..DATA.len() - 1])
    }]);
    let client = check::client().unwrap();
    let (tx, rx) = oneshot::channel();
    let mut tx = Some(tx);
    let result = download::download(
        &client,
        &asset,
        Checksum::Digest(hash(DATA)),
        dir.path(),
        rx,
        |bytes, _| {
            if bytes > 0
                && let Some(tx) = tx.take()
            {
                assert!(dir.path().join(format!("{NAME}.part")).exists());
                tx.send(()).unwrap();
            }
        },
    )
    .await
    .unwrap();
    assert!(result.is_none());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    server.await.unwrap();
}

#[test]
fn swap_preserves_custom_bundle_name_and_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let applications = dir.path().join("Applications");
    let bundle = applications.join("My Shellmate.app");
    let partial = applications.join(".Shellmate-0.1.9.app.partial");
    let old = applications.join(".Shellmate-0.1.8.app.old");
    fs::create_dir_all(&bundle).unwrap();
    fs::write(bundle.join("version"), "old").unwrap();
    assert!(install::swap(&bundle, &partial, &old).is_err());
    assert_eq!(fs::read_to_string(bundle.join("version")).unwrap(), "old");
    assert!(!old.exists());
    fs::create_dir(&partial).unwrap();
    fs::write(partial.join("version"), "new").unwrap();
    fs::create_dir(&old).unwrap();
    assert!(install::swap(&bundle, &partial, &old).is_err());
    fs::remove_dir(&old).unwrap();
    install::swap(&bundle, &partial, &old).unwrap();
    assert_eq!(fs::read_to_string(bundle.join("version")).unwrap(), "new");
    assert!(!partial.exists() && !old.exists());
}

fn fake_bundle(path: &Path, version: &str) {
    let executable = path.join("Contents/MacOS/Shellmate");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::copy("/usr/bin/true", executable).unwrap();
    fs::write(
        path.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.shellmate.Shellmate</string>
<key>CFBundleExecutable</key><string>Shellmate</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
        ),
    )
    .unwrap();
}

#[test]
fn locate_validates_enclosing_bundle_and_translocation() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("Renamed Shellmate.app");
    fake_bundle(&bundle, "0.1.8");
    let exe = bundle.join("Contents/MacOS/Shellmate");
    assert_eq!(install::locate(&exe).unwrap(), bundle);
    assert!(install::locate(dir.path()).is_err());
    assert!(
        install::locate(
            &dir.path()
                .join("AppTranslocation/X/Shellmate.app/Contents/MacOS/X")
        )
        .is_err()
    );
    let plist = bundle.join("Contents/Info.plist");
    fs::write(
        &plist,
        fs::read_to_string(&plist)
            .unwrap()
            .replace("dev.shellmate.Shellmate", "other"),
    )
    .unwrap();
    assert!(install::locate(&exe).is_err());
}

#[tokio::test]
#[ignore = "requires macOS disk image mounting and a loopback listener; see P15 report"]
async fn dmg_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("image");
    let new = source.join("Shellmate.app");
    let running = temp.path().join("Applications/Renamed Shellmate.app");
    fake_bundle(&running, "0.1.8");
    fake_bundle(&new, "0.1.9");
    assert!(
        Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "--sign",
                "-",
                "--identifier",
                "dev.shellmate.Shellmate"
            ])
            .arg(&new)
            .status()
            .unwrap()
            .success()
    );
    let dmg = temp.path().join("fixture.dmg");
    assert!(
        Command::new("/usr/bin/hdiutil")
            .args(["create", "-format", "UDZO", "-srcfolder"])
            .arg(&source)
            .arg(&dmg)
            .status()
            .unwrap()
            .success()
    );
    let bytes = fs::read(dmg).unwrap();
    let mock = Mock::new().await;
    let api = mock.api.clone();
    let server = mock.serve(vec![
        Reply::ok(fixture(&api, &bytes).to_string()),
        Reply::ok(bytes),
    ]);
    let client = check::client().unwrap();
    let Outcome::Release(r, _) = check::latest(&client, &api, &Cache::default())
        .await
        .unwrap()
    else {
        panic!("expected release")
    };
    let asset = r.asset(true).unwrap();
    let cache = temp.path().join("cache");
    let path = fetch(
        &client,
        asset,
        r.checksum(asset).unwrap(),
        &cache,
        |_, _| {},
    )
    .await
    .unwrap()
    .unwrap();
    let bundle = install::locate(&running.join("Contents/MacOS/Shellmate")).unwrap();
    let staged = install::stage(
        &path,
        bundle,
        r.version,
        r#"identifier "dev.shellmate.Shellmate""#.into(),
    )
    .unwrap();
    staged.install().unwrap();
    assert!(
        fs::read_to_string(running.join("Contents/Info.plist"))
            .unwrap()
            .contains("0.1.9")
    );
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn verified_cache_survives_an_interrupted_previous_download() {
    let dir = tempfile::tempdir().unwrap();
    let asset = asset("http://127.0.0.1:1");
    let path = dir.path().join(NAME);
    fs::write(&path, DATA).unwrap();
    fs::write(dir.path().join(format!("{NAME}.part")), b"incomplete").unwrap();
    let mut progress = vec![];
    let result = fetch(
        &check::client().unwrap(),
        &asset,
        Checksum::Digest(hash(DATA)),
        dir.path(),
        |n, total| progress.push((n, total)),
    )
    .await
    .unwrap();
    assert_eq!(result, Some(path));
    assert_eq!(progress, [(asset.size, asset.size)]);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn signed_bundle_is_staged_early_and_reverified_before_swap() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("mounted");
    let new = source.join("Shellmate.app");
    let running = temp.path().join("Applications/Renamed Shellmate.app");
    let version = Version::parse("0.1.9").unwrap();
    let requirement = r#"identifier "dev.shellmate.Shellmate""#;
    fake_bundle(&running, "0.1.8");
    fake_bundle(&new, "0.1.9");
    assert!(
        Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "--sign",
                "-",
                "--identifier",
                "dev.shellmate.Shellmate"
            ])
            .arg(&new)
            .status()
            .unwrap()
            .success()
    );
    let stage = || {
        install::stage_mounted(
            &source,
            running.clone(),
            version.clone(),
            requirement.into(),
        )
    };
    assert!(
        install::stage_mounted(
            &source,
            running.clone(),
            version.clone(),
            r#"identifier "wrong""#.into()
        )
        .is_err()
    );
    assert!(
        install::stage_mounted(
            &source,
            running.clone(),
            Version::new(0, 2, 0),
            requirement.into()
        )
        .is_err()
    );
    let staged = stage().unwrap();
    assert!(stage().is_err());
    let partial = running.with_file_name(".Shellmate-0.1.9.app.partial");
    fs::write(partial.join("Contents/MacOS/Shellmate"), b"tampered").unwrap();
    assert!(staged.install().is_err());
    drop(staged);
    assert!(!partial.exists());
    assert!(
        fs::read_to_string(running.join("Contents/Info.plist"))
            .unwrap()
            .contains("0.1.8")
    );
    stage().unwrap().install().unwrap();
    assert!(
        fs::read_to_string(running.join("Contents/Info.plist"))
            .unwrap()
            .contains("0.1.9")
    );
}

#[test]
fn trusted_certificate_list_allows_comments_and_successors() {
    let list = "# trusted\n8725A95483477EA813F288B237718BAEBDF96FEA  current\n\n\
                0123456789abcdef0123456789abcdef01234567 successor # added before rotating\n";
    assert_eq!(
        install::trusted_requirement(list).unwrap(),
        "identifier \"dev.shellmate.Shellmate\" and (certificate leaf = H\"8725a95483477ea813f288b237718baebdf96fea\" \
         or certificate leaf = H\"0123456789abcdef0123456789abcdef01234567\")"
    );
}

#[test]
fn trusted_certificate_list_rejects_placeholders_and_typos() {
    for list in [
        "",
        "# nothing yet\n",
        "0000000000000000000000000000000000000000\n",
        "8725a9548347\n",
        "8725a95483477ea813f288b237718baebdf96fea\nnot-a-fingerprint\n",
    ] {
        assert!(install::trusted_requirement(list).is_err(), "{list:?}");
    }
}

#[test]
fn shipped_trusted_certificate_list_is_valid() {
    let list = include_str!("../../assets/signing/trusted-certificates.txt");
    assert!(install::trusted_requirement(list).unwrap().contains("8725a95483477ea813f288b237718baebdf96fea"));
}
