//! How sizes and permissions are written.

/// `612 B`, `2.3 KB`, `1.8 MB`, `1.21 GB`.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

/// The permission bits as `ls -l` writes them: `rwxr-xr-x`, with `s`/`S` for setuid and setgid
/// and `t`/`T` for the sticky bit (capital where the execute bit is missing).
pub fn mode_string(mode: u32) -> String {
    let bit = |mask: u32, ch: char| if mode & mask != 0 { ch } else { '-' };
    let exec = |x: u32, special: u32, set: char, unset: char| match (mode & x != 0, mode & special != 0) {
        (true, true) => set,
        (false, true) => unset,
        (true, false) => 'x',
        (false, false) => '-',
    };
    [
        bit(0o400, 'r'),
        bit(0o200, 'w'),
        exec(0o100, 0o4000, 's', 'S'),
        bit(0o040, 'r'),
        bit(0o020, 'w'),
        exec(0o010, 0o2000, 's', 'S'),
        bit(0o004, 'r'),
        bit(0o002, 'w'),
        exec(0o001, 0o1000, 't', 'T'),
    ]
    .iter()
    .collect()
}

/// The permission bits in octal: `644`, and `4755` when a special bit is set.
pub fn mode_octal(mode: u32) -> String {
    let bits = mode & 0o7777;
    if bits > 0o777 { format!("{bits:04o}") } else { format!("{:03o}", bits & 0o777) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_the_prototype_format() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(612), "612 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(2355), "2.3 KB");
        assert_eq!(format_size(1_887_437), "1.8 MB");
        assert_eq!(format_size(1_298_000_000), "1.21 GB");
    }

    #[test]
    fn permissions_read_like_ls() {
        assert_eq!(mode_string(0o755), "rwxr-xr-x");
        assert_eq!(mode_string(0o644), "rw-r--r--");
        assert_eq!(mode_string(0o600), "rw-------");
        assert_eq!(mode_string(0o000), "---------");
        assert_eq!(mode_string(0o777), "rwxrwxrwx");
        assert_eq!(mode_string(0o100644), "rw-r--r--", "file type bits are not permissions");
        assert_eq!(mode_string(0o4755), "rwsr-xr-x");
        assert_eq!(mode_string(0o4644), "rwSr--r--");
        assert_eq!(mode_string(0o2755), "rwxr-sr-x");
        assert_eq!(mode_string(0o1777), "rwxrwxrwt");
        assert_eq!(mode_string(0o1776), "rwxrwxrwT");
    }

    #[test]
    fn octal_has_three_digits_and_four_with_special_bits() {
        assert_eq!(mode_octal(0o644), "644");
        assert_eq!(mode_octal(0o7), "007");
        assert_eq!(mode_octal(0o100755), "755");
        assert_eq!(mode_octal(0o4755), "4755");
        assert_eq!(mode_octal(0o1777), "1777");
    }
}
