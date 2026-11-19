use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use std::{
    ffi::CString,
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const IDENTIFIER: &str = "dev.shellmate.Shellmate";
const TRUSTED: &str = include_str!("../../assets/signing/trusted-certificates.txt");
const HDIUTIL: &str = "/usr/bin/hdiutil";

pub fn requirement() -> Result<String> {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("SHELLMATE_UPDATE_REQUIREMENT") {
        ensure!(!value.trim().is_empty(), "签名要求为空，请前往下载页。");
        return Ok(value);
    }
    trusted_requirement(TRUSTED)
}

/// Shellmate's identifier signed by any certificate in the trusted list. Several entries let a release
/// signed with the current certificate also trust its successor, so a certificate can be replaced
/// without stranding installed copies (docs/signing.md). scripts/signing-requirement.sh builds the
/// same string for the release workflow.
pub(super) fn trusted_requirement(list: &str) -> Result<String> {
    let pins: Vec<String> = list
        .lines()
        .filter_map(|line| line.split('#').next()?.split_whitespace().next())
        .map(str::to_ascii_lowercase)
        .collect();
    ensure!(
        !pins.is_empty()
            && pins.iter().all(|pin| {
                pin.len() == 40
                    && pin.bytes().all(|b| b.is_ascii_hexdigit())
                    && pin.bytes().any(|b| b != b'0')
            }),
        "此版本尚未配置更新签名，请前往下载页。"
    );
    let leaves = pins
        .iter()
        .map(|pin| format!("certificate leaf = H\"{pin}\""))
        .collect::<Vec<_>>()
        .join(" or ");
    Ok(format!("identifier \"{IDENTIFIER}\" and ({leaves})"))
}

fn run(command: &mut Command, message: &'static str) -> Result<Vec<u8>> {
    let output = command.stdin(Stdio::null()).output().context(message)?;
    if !output.status.success() {
        log::warn!(
            "{}: {}",
            message,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        bail!(message)
    }
    Ok(output.stdout)
}

fn plist(app: &Path, key: &str) -> Result<String> {
    let bytes = run(
        Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-n", "-o", "-", "--"])
            .arg(app.join("Contents/Info.plist")),
        "无法读取应用信息",
    )?;
    String::from_utf8(bytes).context("应用信息不是有效文本")
}

pub fn locate(exe: &Path) -> Result<PathBuf> {
    let bundle = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|x| x == "app"))
        .context("请从已安装的 Shellmate 应用中更新。")?;
    ensure!(
        !bundle
            .components()
            .any(|c| c.as_os_str() == "AppTranslocation"),
        "应用位于隔离位置，请前往下载页。"
    );
    ensure!(
        plist(bundle, "CFBundleIdentifier")? == IDENTIFIER,
        "应用标识不匹配，请前往下载页。"
    );
    let parent = bundle.parent().context("找不到应用目录")?;
    let path = CString::new(parent.as_os_str().as_bytes())?;
    let mut info = std::mem::MaybeUninit::uninit();
    // SAFETY: path is NUL terminated and statvfs initializes info on success.
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } == 0,
        "无法读取应用卷信息。"
    );
    ensure!(
        unsafe { info.assume_init() }.f_flag & libc::ST_RDONLY == 0,
        "应用位于只读卷，请前往下载页。"
    );
    ensure!(
        unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) } == 0,
        "应用所在目录不可写，请前往下载页。"
    );
    Ok(bundle.to_owned())
}

pub fn target() -> Result<(PathBuf, String)> {
    let bundle = locate(&std::env::current_exe().context("找不到正在运行的应用")?)?;
    Ok((bundle, requirement()?))
}

fn verify(app: &Path, version: &Version, requirement: &str) -> Result<()> {
    ensure!(
        fs::symlink_metadata(app).is_ok_and(|m| m.is_dir()),
        "安装包中缺少 Shellmate 应用"
    );
    ensure!(
        plist(app, "CFBundleIdentifier")? == IDENTIFIER,
        "安装包的应用标识不匹配"
    );
    ensure!(
        plist(app, "CFBundleShortVersionString")? == version.to_string(),
        "安装包版本与发布版本不符"
    );
    run(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(format!("-R={requirement}"))
            .arg(app),
        "应用签名校验失败",
    )?;
    Ok(())
}

struct Mount {
    dir: PathBuf,
    active: bool,
}
impl Mount {
    fn attach(image: &Path) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("shellmate-update-")
            .tempdir()
            .context("无法创建挂载点")?;
        // Use a non-recursive remove_dir in Drop, even if forced detach fails.
        let mount = Self {
            dir: dir.keep(),
            active: true,
        };
        run(
            Command::new(HDIUTIL)
                .args([
                    "attach",
                    "-nobrowse",
                    "-readonly",
                    "-noautoopen",
                    "-mountpoint",
                ])
                .arg(&mount.dir)
                .arg(image),
            "无法挂载安装包",
        )?;
        Ok(mount)
    }

    fn detach(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        run(
            Command::new(HDIUTIL).arg("detach").arg(&self.dir),
            "无法卸载安装包",
        )
        .or_else(|_| {
            run(
                Command::new(HDIUTIL)
                    .args(["detach", "-force"])
                    .arg(&self.dir),
                "强制卸载安装包失败",
            )
        })?;
        self.active = false;
        Ok(())
    }
}
impl Drop for Mount {
    fn drop(&mut self) {
        let _ = self.detach();
        let _ = fs::remove_dir(&self.dir);
    }
}

pub struct Staged {
    pub bundle: PathBuf,
    partial: PathBuf,
    old: PathBuf,
    version: Version,
    requirement: String,
    original: fs::File,
    // Staging and backup names are shared by all copies in this directory.
    _directory_lock: fs::File,
}
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.partial);
    }
}

pub fn stage(
    image: &Path,
    bundle: PathBuf,
    version: Version,
    requirement: String,
) -> Result<Staged> {
    let mut mount = Mount::attach(image)?;
    let staged = stage_mounted(&mount.dir, bundle, version, requirement)?;
    mount.detach()?;
    Ok(staged)
}

pub(super) fn stage_mounted(
    mount: &Path,
    bundle: PathBuf,
    version: Version,
    requirement: String,
) -> Result<Staged> {
    let source = mount.join("Shellmate.app");
    verify(&source, &version, &requirement)?;
    let parent = bundle.parent().context("找不到应用目录")?;
    let directory_lock = fs::File::open(parent).context("无法打开应用目录")?;
    directory_lock.try_lock().context("另一个更新正在安装")?;
    let original = fs::File::open(&bundle).context("无法打开当前应用")?;
    let old_version = Version::parse(&plist(&bundle, "CFBundleShortVersionString")?)
        .context("当前应用版本无效")?;
    ensure!(
        version.cmp_precedence(&old_version).is_gt(),
        "已安装的版本已发生变化，请重新启动"
    );
    let partial = bundle.with_file_name(format!(".Shellmate-{version}.app.partial"));
    let old = bundle.with_file_name(format!(".Shellmate-{old_version}.app.old"));
    ensure!(bundle != partial && bundle != old, "无法确定更新目录");
    if partial.try_exists()? {
        fs::remove_dir_all(&partial).context("无法清理上次暂存的应用")?;
    }
    fs::create_dir(&partial).context("无法暂存应用，请检查目录权限")?;
    let staged = Staged {
        bundle,
        partial,
        old,
        version,
        requirement,
        original,
        _directory_lock: directory_lock,
    };
    run(
        Command::new("/usr/bin/ditto")
            .arg(source)
            .arg(&staged.partial),
        "暂存新版应用失败",
    )?;
    Ok(staged)
}

impl Staged {
    pub fn install(&self) -> Result<()> {
        // Detect replacement of the running bundle by another installer or Finder.
        let locked = self.original.metadata().context("无法确认当前应用")?;
        let current = fs::symlink_metadata(&self.bundle).context("正在运行的应用已移动")?;
        ensure!(
            (locked.dev(), locked.ino()) == (current.dev(), current.ino()),
            "正在运行的应用已被替换"
        );
        verify(&self.partial, &self.version, &self.requirement)?;
        swap(&self.bundle, &self.partial, &self.old)
    }
}

pub(super) fn swap(bundle: &Path, partial: &Path, old: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(old).is_err_and(|e| e.kind() == io::ErrorKind::NotFound),
        "旧版备份路径已被占用"
    );
    fs::rename(bundle, old).context("无法备份当前版本")?;
    if let Err(err) = fs::rename(partial, bundle) {
        fs::rename(old, bundle)
            .with_context(|| format!("恢复失败，旧版保存在 {}", old.display()))?;
        return Err(err).context("安装失败，已恢复当前版本");
    }
    if let Err(e) = fs::remove_dir_all(old) {
        log::warn!("could not remove old bundle: {e}");
    }
    Ok(())
}

pub fn relaunch(bundle: &Path) -> Result<()> {
    // Positional arguments preserve arbitrary bundle names, including shell metacharacters.
    Command::new("/bin/sh")
        .args([
            "-c",
            r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open "$2""#,
            "shellmate-relaunch",
        ])
        .arg(std::process::id().to_string())
        .arg(bundle)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("更新已安装，请手动重新启动 Shellmate")?;
    Ok(())
}
