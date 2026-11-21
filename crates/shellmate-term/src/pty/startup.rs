//! macOS can briefly report the forked parent's name after PTY spawn returns.
use std::mem::{MaybeUninit, size_of};

// macOS SDK sys/proc_info.h: "process has called exec". libc exposes the
// proc_bsdinfo structure and flavor, but not this flag.
pub(super) const PROC_FLAG_EXEC: u32 = 0x4000;

pub(super) fn process_name(pid: i32) -> Option<String> {
    name_after_exec(&snapshot(pid)?)
}

pub(super) fn snapshot(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: the buffer is exactly the size required by PROC_PIDTBSDINFO.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled the complete structure above. Reading flags
    // and name from one snapshot avoids racing two separate process queries.
    Some(unsafe { info.assume_init() })
}

pub(super) fn name_after_exec(info: &libc::proc_bsdinfo) -> Option<String> {
    if info.pbi_flags & PROC_FLAG_EXEC == 0 {
        return None;
    }
    let name = if info.pbi_name[0] == 0 {
        &info.pbi_comm[..]
    } else {
        &info.pbi_name[..]
    };
    let bytes: Vec<u8> = name
        .iter()
        .copied()
        .take_while(|byte| *byte != 0)
        .map(|byte| byte as u8)
        .collect();
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &[u8], flags: u32) -> libc::proc_bsdinfo {
        // SAFETY: proc_bsdinfo contains only integer fields and integer arrays.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        info.pbi_flags = flags;
        for (destination, byte) in info.pbi_name.iter_mut().zip(name) {
            *destination = *byte as libc::c_char;
        }
        info
    }

    #[test]
    fn inherited_parent_name_is_hidden_but_same_program_after_exec_is_valid() {
        for name in [
            b"shellmate".as_slice(),
            b"renamed-debug-app",
            b"zsh",
            b"fish",
        ] {
            assert_eq!(name_after_exec(&snapshot(name, 0xF0)), None);
            assert_eq!(
                name_after_exec(&snapshot(name, 0xF0 | PROC_FLAG_EXEC)).as_deref(),
                std::str::from_utf8(name).ok()
            );
        }
    }

    #[test]
    fn missing_names_use_comm_and_full_buffers_need_no_terminator() {
        let mut info = snapshot(b"", PROC_FLAG_EXEC);
        assert_eq!(name_after_exec(&info), None);
        for (destination, byte) in info.pbi_comm.iter_mut().zip(b"zsh") {
            *destination = *byte as libc::c_char;
        }
        assert_eq!(name_after_exec(&info).as_deref(), Some("zsh"));
        info.pbi_name.fill(b'x' as libc::c_char);
        assert_eq!(
            name_after_exec(&info),
            Some("x".repeat(info.pbi_name.len()))
        );
    }

    #[test]
    fn current_executable_is_accepted_and_missing_pid_is_unavailable() {
        assert!(process_name(std::process::id() as i32).is_some());
        assert_eq!(process_name(-1), None);
    }
}
