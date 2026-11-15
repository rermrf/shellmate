//! A pretend `/proc` and `/sys` and a bin directory of stand-in commands, so that the real collector
//! script can run on any machine (also on a Mac) under real shells.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn fixture(name: &str) -> String {
    fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// Where a command really is (searched in the real PATH), for the stand-ins that only record a call.
pub fn real(command: &str) -> PathBuf {
    std::env::var("PATH")
        .unwrap()
        .split(':')
        .map(|dir| Path::new(dir).join(command))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("no {command} on this machine"))
}

pub struct FakeRoot {
    pub dir: tempfile::TempDir,
}

/// Which flavour of `ps` the stand-in speaks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ps {
    /// `ps -eo pid=,uid=,args=` works; the BusyBox form does not exist.
    Procps,
    /// Only `ps -o pid=,user=,args=` works; `-e` and `uid` are refused (BusyBox 1.30 style).
    Busybox,
    /// There is no usable `ps`.
    Missing,
}

impl FakeRoot {
    pub fn new(ps: Ps) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let proc = root.join("proc");
        fs::create_dir_all(proc.join("net")).unwrap();
        let write = |path: PathBuf, text: &str| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write(
            proc.join("stat"),
            &format!(
                "{}intr 1 2 3 4 5\nctxt 99\nbtime 1700000000\nprocesses 10\n",
                fixture("procps/proc_stat_cpu.txt")
            ),
        );
        write(proc.join("uptime"), &fixture("procps/uptime.txt"));
        write(proc.join("loadavg"), &fixture("procps/loadavg.txt"));
        write(proc.join("meminfo"), &fixture("procps/meminfo.txt"));
        write(proc.join("net/route"), &fixture("procps/net_route.txt"));
        write(
            proc.join("net/dev"),
            &(fixture("procps/net_dev.txt")
                + "docker0: 111 1 0 0 0 0 0 0 222 1 0 0 0 0 0 0\n veth1: 5 1 0 0 0 0 0 0 6 1 0 0 0 0 0 0\n"),
        );
        write(
            proc.join("mounts"),
            "overlay / overlay rw 0 0\nproc /proc proc rw 0 0\n/dev/vdb1 /config ext4 rw 0 0\n/dev/sda9 /mnt/with\\040space ext4 rw 0 0\ntmpfs /run tmpfs rw 0 0\nserver:/export /nfs nfs4 rw 0 0\n/dev/sdb1 /data xfs rw 0 0\n/dev/sdc1 /boot ext4 rw 0 0\n/dev/loop3 /snap/core/1 squashfs ro 0 0\nsysfs /sys sysfs rw 0 0\nsshfs#me@host: /mnt/remote fuse.sshfs rw 0 0\nmac /mnt/mac virtiofs rw 0 0\nmac /Users virtiofs rw 0 0\n/dev/vdb1 /config/bind btrfs rw 0 0\n/dev/sdb1 /data/again xfs rw 0 0\n",
        );
        for line in fixture("procps/proc_stat_lines.txt").lines() {
            let pid = line.split_whitespace().next().unwrap();
            write(proc.join(pid).join("stat"), &format!("{line}\n"));
        }
        // Things in /proc that are not processes.
        write(proc.join("sys/stat"), "not a process\n");
        write(proc.join("self/stat"), "not a process\n");
        let sys = root.join("sys");
        write(sys.join("class/thermal/thermal_zone0/temp"), "45000\n");
        write(sys.join("class/thermal/thermal_zone0/type"), "x86_pkg_temp\n");
        write(sys.join("class/thermal/thermal_zone1/temp"), "38500\n");
        write(sys.join("class/thermal/thermal_zone1/type"), "acpitz\n");

        // The stand-ins: each records its call in `calls.log` (one line per fork).
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let stub = |name: &str, body: &str| {
            let path = bin.join(name);
            fs::write(&path, format!("#!/bin/sh\necho \"{name} $*\" >> \"$CALLS\"\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        stub("sleep", &format!("exec {} \"$@\"", real("sleep").display()));
        stub("cut", &format!("exec {} \"$@\"", real("cut").display()));
        let procps_ps = fixture("procps/ps.txt");
        let busybox_ps = fixture("busybox/ps_noheader.txt");
        write(root.join("ps_procps.txt"), &procps_ps);
        write(root.join("ps_busybox.txt"), &busybox_ps);
        let ps_body = match ps {
            Ps::Procps => format!(
                "case \"$*\" in '-eo pid=,uid=,args=') cat \"{0}/ps_procps.txt\";; *) echo \"ps: bad option\" >&2; exit 1;; esac",
                root.display()
            ),
            Ps::Busybox => format!(
                "case \"$*\" in '-o pid=,user=,args=') cat \"{0}/ps_busybox.txt\";; *) echo \"ps: bad -o argument\" >&2; exit 1;; esac",
                root.display()
            ),
            Ps::Missing => "exit 127".to_string(),
        };
        stub("ps", &ps_body);
        write(root.join("df.txt"), &fixture("procps/df.txt"));
        stub("df", &format!("cat \"{}/df.txt\"", root.display()));
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn proc_root(&self) -> String {
        self.path().join("proc").display().to_string()
    }

    pub fn sys_root(&self) -> String {
        self.path().join("sys").display().to_string()
    }

    pub fn calls_log(&self) -> PathBuf {
        self.path().join("calls.log")
    }

    pub fn bin(&self) -> PathBuf {
        self.path().join("bin")
    }
}

/// A bin directory for running the probe on the developer's machine: only the tools the probe needs (as
/// links to the real ones), and a `sudo`, `docker` and `podman` that do nothing but fail and note that
/// they were called. A test must never run the real `sudo` or `docker`: the PATH of the probe is this
/// directory and nothing else.
pub struct ProbeBin {
    pub dir: tempfile::TempDir,
}

impl ProbeBin {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for tool in ["sh", "uname", "id", "head", "getconf"] {
            if let Some(path) = std::env::var("PATH").unwrap().split(':').map(|d| Path::new(d).join(tool)).find(|p| p.is_file()) {
                std::os::unix::fs::symlink(path, dir.path().join(tool)).unwrap();
            }
        }
        for fake in ["sudo", "docker", "podman"] {
            let path = dir.path().join(fake);
            fs::write(&path, format!("#!/bin/sh\necho \"{fake} $*\" >> \"$CALLS\"\nexit 1\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn calls_log(&self) -> PathBuf {
        self.dir.path().join("calls.log")
    }
}
