#![allow(deprecated, reason = "Use the existing libc Mach bindings without adding mach2")]

use std::collections::BTreeMap;
use std::ffi::{CStr, c_int};
use std::mem::{MaybeUninit, size_of};
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::parse::LoadAvg;
use crate::{Disk, MemoryCard, Probe};

fn sysctl_bytes(name: &CStr, bytes: &mut [u8]) -> Option<usize> {
    let mut len = bytes.len();
    let status = unsafe { libc::sysctlbyname(name.as_ptr(), bytes.as_mut_ptr().cast(), &mut len, ptr::null_mut(), 0) };
    (status == 0 && len <= bytes.len()).then_some(len)
}

unsafe fn sysctl_value<T: Copy>(name: &CStr) -> Option<T> {
    let mut out = MaybeUninit::<T>::uninit();
    let mut len = size_of::<T>();
    let status = unsafe { libc::sysctlbyname(name.as_ptr(), out.as_mut_ptr().cast(), &mut len, ptr::null_mut(), 0) };
    (status == 0 && len == size_of::<T>()).then(|| unsafe { out.assume_init() })
}

fn sysctl_string(name: &CStr) -> Option<String> {
    let mut bytes = [0; 512];
    let len = sysctl_bytes(name, &mut bytes)?;
    Some(
        CStr::from_bytes_until_nul(&bytes[..len])
            .ok()?
            .to_string_lossy()
            .into_owned(),
    )
}

pub(crate) fn host_probe() -> Probe {
    Probe {
        local_hardware: Some(
            [sysctl_string(c"machdep.cpu.brand_string"), sysctl_string(c"hw.machine")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
        ),
        os_name: Some(sysctl_string(c"kern.osproductversion").map_or_else(|| "macOS".into(), |v| format!("macOS {v}"))),
        kernel: format!("Darwin {}", sysctl_string(c"kern.osrelease").unwrap_or_default()),
        uptime_secs: uptime(),
        uid: Some(unsafe { libc::geteuid() }),
        sudo: crate::Sudo::NeedsPassword,
        ..Probe::unknown()
    }
}

pub(super) fn uptime() -> Option<f64> {
    let boot = unsafe { sysctl_value::<libc::timeval>(c"kern.boottime") }?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
    Some((now - boot.tv_sec as f64 - f64::from(boot.tv_usec) / 1e6).max(0.0))
}

pub(super) fn load() -> Option<LoadAvg> {
    let mut values = [0.; 3];
    (unsafe { libc::getloadavg(values.as_mut_ptr(), 3) } == 3).then(|| LoadAvg {
        one: values[0] as f32,
        five: values[1] as f32,
        fifteen: values[2] as f32,
        running: None,
        threads: None,
    })
}

unsafe extern "C" {
    fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::kern_return_t;
}

pub(super) struct HostPort(pub(super) libc::mach_port_t);

impl Default for HostPort {
    fn default() -> Self {
        Self(unsafe { libc::mach_host_self() })
    }
}

impl Drop for HostPort {
    fn drop(&mut self) {
        unsafe { mach_port_deallocate(libc::mach_task_self(), self.0) };
    }
}

pub(super) fn cpu(host: &HostPort) -> Option<Vec<[u32; 4]>> {
    let (mut cores, mut raw, mut count) = (0, ptr::null_mut(), 0);
    let status =
        unsafe { libc::host_processor_info(host.0, libc::PROCESSOR_CPU_LOAD_INFO, &mut cores, &mut raw, &mut count) };
    if status != 0 || raw.is_null() {
        return None;
    }
    let result = (count as usize == cores as usize * 4)
        .then(|| unsafe { std::slice::from_raw_parts(raw.cast::<[u32; 4]>(), cores as usize) }.to_vec());
    unsafe {
        libc::vm_deallocate(
            libc::mach_task_self(),
            raw as usize,
            count as usize * size_of::<libc::integer_t>(),
        )
    };
    result
}

/// Activity Monitor: used = (internal - purgeable + wired + physical compressor pages) * page size.
/// File-backed and purgeable pages are reclaimable cache, not used application memory.
fn memory_card(vm: &libc::vm_statistics64, page: u64, total: u64) -> MemoryCard {
    let app = u64::from(vm.internal_page_count.saturating_sub(vm.purgeable_count));
    let used = ((app + u64::from(vm.wire_count) + u64::from(vm.compressor_page_count)) * page).min(total);
    let available = total - used;
    let cache = ((u64::from(vm.external_page_count) + u64::from(vm.purgeable_count)) * page).min(available);
    MemoryCard {
        total,
        used,
        available,
        cache,
        ..Default::default()
    }
}

pub(super) fn memory(host: &HostPort) -> Option<MemoryCard> {
    let mut vm = unsafe { std::mem::zeroed::<libc::vm_statistics64>() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let status = unsafe { libc::host_statistics64(host.0, libc::HOST_VM_INFO64, (&raw mut vm).cast(), &mut count) };
    if status != 0 {
        return None;
    }
    let total = unsafe { sysctl_value::<u64>(c"hw.memsize") }?;
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    let mut card = memory_card(&vm, page, total);
    if let Some(swap) = unsafe { sysctl_value::<libc::xsw_usage>(c"vm.swapusage") } {
        card.swap_total = swap.xsu_total;
        card.swap_used = swap.xsu_used;
    }
    Some(card)
}

pub(super) type Interfaces = BTreeMap<u16, (u64, u64)>;

pub(super) fn network() -> Option<Interfaces> {
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let mut read = |buf, len| unsafe { libc::sysctl(mib.as_mut_ptr(), 6, buf, len, ptr::null_mut(), 0) };
    let mut len = 0;
    if read(ptr::null_mut(), &mut len) != 0 {
        return None;
    }
    let mut bytes = vec![0u8; len];
    if read(bytes.as_mut_ptr().cast(), &mut len) != 0 {
        return None;
    }
    let mut rest = bytes.get(..len)?;
    let mut interfaces = Interfaces::new();
    while rest.len() >= 4 {
        let length = u16::from_ne_bytes([rest[0], rest[1]]) as usize;
        if length < 4 || length > rest.len() {
            return None;
        }
        if rest[3] == libc::RTM_IFINFO2 as u8 && length >= size_of::<libc::if_msghdr2>() {
            let header = unsafe { rest.as_ptr().cast::<libc::if_msghdr2>().read_unaligned() };
            if header.ifm_flags & libc::IFF_LOOPBACK == 0 {
                interfaces.insert(
                    header.ifm_index,
                    (header.ifm_data.ifi_ibytes, header.ifm_data.ifi_obytes),
                );
            }
        }
        rest = &rest[length..];
    }
    Some(interfaces)
}

fn mount_label<'a>(mount: &'a str, kind: &str, flags: u32) -> Option<&'a str> {
    if flags & libc::MNT_LOCAL as u32 == 0 || matches!(kind, "devfs" | "autofs" | "map" | "fdesc") {
        return None;
    }
    if matches!(mount, "/" | "/System/Volumes/Data") {
        return Some("/");
    }
    if flags & libc::MNT_DONTBROWSE as u32 != 0 {
        return None;
    }
    let name = mount.strip_prefix("/Volumes/")?;
    (!name.is_empty() && !name.starts_with('.') && !name.contains('/') && flags & libc::MNT_SNAPSHOT as u32 == 0)
        .then_some(mount)
}

pub(super) fn disks() -> Option<Vec<Disk>> {
    let count = unsafe { libc::getfsstat(ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 1 {
        return None;
    }
    let mut mounts = Vec::<libc::statfs>::with_capacity(count as usize + 8);
    let bytes = c_int::try_from(mounts.capacity() * size_of::<libc::statfs>()).ok()?;
    let count = usize::try_from(unsafe { libc::getfsstat(mounts.as_mut_ptr(), bytes, libc::MNT_NOWAIT) }).ok()?;
    unsafe { mounts.set_len(count.min(mounts.capacity())) };
    let mut visible = BTreeMap::new();
    for fs in mounts {
        let mount = unsafe { CStr::from_ptr(fs.f_mntonname.as_ptr()) }.to_string_lossy();
        let kind = unsafe { CStr::from_ptr(fs.f_fstypename.as_ptr()) }.to_string_lossy();
        let Some(label) = mount_label(&mount, &kind, fs.f_flags) else {
            continue;
        };
        if mount == "/" && visible.contains_key("/") {
            continue;
        }
        let block = u64::from(fs.f_bsize);
        let total = fs.f_blocks.saturating_mul(block);
        let available = fs.f_bavail.saturating_mul(block).min(total);
        let used = total.saturating_sub(fs.f_bfree.saturating_mul(block));
        let device = unsafe { CStr::from_ptr(fs.f_mntfromname.as_ptr()) };
        visible.insert(
            label.to_owned(),
            Disk {
                mount: label.into(),
                filesystem: device.to_string_lossy().into_owned(),
                total,
                used,
                available,
                percent: (100. * used as f64 / total.max(1) as f64).round() as u8,
            },
        );
    }
    Some(visible.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_monitor_memory_formula() {
        let mut vm = unsafe { std::mem::zeroed::<libc::vm_statistics64>() };
        vm.internal_page_count = 50;
        vm.purgeable_count = 8;
        vm.wire_count = 10;
        vm.compressor_page_count = 4;
        vm.external_page_count = 15;
        let card = memory_card(&vm, 16384, 100 * 16384);
        assert_eq!(
            (card.used, card.cache, card.available),
            (56 * 16384, 23 * 16384, 44 * 16384)
        );
    }

    #[test]
    fn only_user_visible_local_volumes_and_one_root() {
        let local = libc::MNT_LOCAL as u32;
        assert_eq!(mount_label("/", "apfs", local | libc::MNT_SNAPSHOT as u32), Some("/"));
        assert_eq!(mount_label("/System/Volumes/Data", "apfs", local), Some("/"));
        assert_eq!(mount_label("/Volumes/My SSD", "apfs", local), Some("/Volumes/My SSD"));
        for (mount, kind, flags) in [
            ("/System/Volumes/VM", "apfs", local),
            ("/Volumes/share", "smbfs", 0),
            ("/Volumes/test", "autofs", local),
            ("/Volumes/snap", "apfs", local | libc::MNT_SNAPSHOT as u32),
            ("/dev", "devfs", local),
        ] {
            assert_eq!(mount_label(mount, kind, flags), None);
        }
    }
}
