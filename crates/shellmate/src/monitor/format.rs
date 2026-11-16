//! How the monitor writes numbers: sizes, rates, percentages, durations.

/// `9.6 GB`, `128 MB`, `812 KB`, `0 B`: a size in the units of the prototype's cards.
pub fn bytes(value: u64) -> String {
    const KB: f64 = 1024.0;
    let b = value as f64;
    if b >= KB.powi(4) {
        let (divisor,unit)=if b>=KB.powi(6){(KB.powi(6),"EB")}else if b>=KB.powi(5){(KB.powi(5),"PB")}else{(KB.powi(4),"TB")};
        format!("{:.1} {unit}",b/divisor)
    } else if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.0} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{value} B")
    }
}

/// A size for the totals of a disk or the memory card, always with a decimal from a gigabyte up
/// (`79 GB`, `1.0 GB`, `0.2 GB`): `51 / 79 GB` is how the prototype writes it.
pub fn gigabytes(value: u64) -> String {
    if value >= 1<<40 {return bytes(value);}
    let gb = value as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else if gb >= 0.1 {
        format!("{gb:.1} GB")
    } else {
        bytes(value)
    }
}

/// One size as the number of a pair like `51 / 79 GB`: a decimal below `decimals_below` gigabytes (and none for
/// `1.0`), a whole number from there up.
fn gb_number(value: u64, decimals_below: f64) -> String {
    let gb = value as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= decimals_below { format!("{gb:.0}") } else { format!("{gb:.1}").trim_end_matches(".0").to_string() }
}

/// `51 / 79 GB`, `9.6 / 15.6 GB`, `0.2 / 1 GB` (`50 / 80 MB` when the total is below a tenth of a gigabyte): used
/// and total with the unit once. Disks are written in whole gigabytes from 10 up, memory from 100 up.
pub fn gb_pair(used: u64, total: u64, decimals_below: f64) -> String {
    if total >= 1<<40 {
        let shift=if total>=1<<60{60}else if total>=1<<50{50}else{40};
        let unit=match shift{60=>"EB",50=>"PB",_=>"TB"};
        return format!("{:.1} / {:.1} {unit}",used as f64/(1u64<<shift) as f64,total as f64/(1u64<<shift)as f64);
    }
    if (total as f64) < 0.1 * 1024.0 * 1024.0 * 1024.0 {
        return format!("{:.0} / {:.0} MB", used as f64 / 1_048_576.0, total as f64 / 1_048_576.0);
    }
    format!("{} / {} GB", gb_number(used, decimals_below), gb_number(total, decimals_below))
}

/// A size in decimal units, the way docker writes image sizes: `3.2 GB`, `29 MB`, `7.8 MB`, `812 kB`.
pub fn decimal_size(value: u64) -> String {
    let b = value as f64;
    if b >= 1e12 { format!("{:.1} TB",b/1e12) } else if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e7 {
        format!("{:.0} MB", b / 1e6)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} kB", b / 1e3)
    } else {
        format!("{value} B")
    }
}

/// A size without a decimal where it is a whole number of units, for a memory limit: `1 GB`, `256 MB`, `1.5 GB`.
pub fn bytes_short(value: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = value as f64;
    if value >= 1<<40 { return bytes(value); }
    if b >= 1024.0 * MB {
        let gb = b / (1024.0 * MB);
        if (gb - gb.round()).abs() < 0.05 { format!("{:.0} GB", gb.round()) } else { format!("{gb:.1} GB") }
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        bytes(value)
    }
}

/// The resident memory of a process: `532 MB`, `5 MB`, `812 KB`, `1.2 GB`.
pub fn rss(value: u64) -> String {
    bytes(value)
}

/// A rate with the unit the larger of two rates asks for: `("0.4", "2.4", "MB/s")`.
pub fn rate_pair(rx: f64, tx: f64) -> (String, String, &'static str) {
    const KB: f64 = 1024.0;
    let top = rx.max(tx).max(0.0);
    let (divisor, unit) = if top >= KB * KB * KB {
        (KB * KB * KB, "GB/s")
    } else if top >= KB * KB {
        (KB * KB, "MB/s")
    } else if top >= KB {
        (KB, "KB/s")
    } else {
        (1.0, "B/s")
    };
    let text = |value: f64| {
        let v = value.max(0.0) / divisor;
        if unit == "B/s" || v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") }
    };
    (text(rx), text(tx), unit)
}

/// A percentage as a card shows it (`19`), or `—` while there is none.
pub fn percent_whole(value: Option<f32>) -> String {
    value.map_or_else(|| "—".into(), |v| format!("{:.0}", v.clamp(0.0, 100.0)))
}

/// A percentage as a table cell shows it (`13.0`), or `—`.
pub fn percent_cell(value: Option<f32>) -> String {
    value.map_or_else(|| "—".into(), |v| format!("{:.1}", v.max(0.0)))
}

/// How long a server has been up: `41 天`, `3 小时 12 分`, `12 分钟`, `刚启动`.
pub fn uptime(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    let (days, hours, minutes) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60);
    if days > 0 {
        format!("{days} 天")
    } else if hours > 0 {
        format!("{hours} 小时 {minutes} 分")
    } else if minutes > 0 {
        format!("{minutes} 分钟")
    } else {
        "刚启动".into()
    }
}

/// `0.42 0.38 0.31`.
pub fn load(one: f32, five: f32, fifteen: f32) -> String {
    format!("{one:.2} {five:.2} {fifteen:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_server_sizes_and_rates_stay_compact_with_units() {
        assert_eq!(bytes(1_319_413_953_331),"1.2 TB");
        assert_eq!(bytes(u64::MAX),"16.0 EB");
        assert_eq!(gb_pair(1<<40,2<<40,10.),"1.0 / 2.0 TB");
        assert_eq!(rate_pair(12.5*(1u64<<30)as f64,0.),("12.5".into(),"0.0".into(),"GB/s"));
        assert_eq!(decimal_size(1_200_000_000_000),"1.2 TB");
    }
    #[test]
    fn sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(812), "812 B");
        assert_eq!(bytes(1024), "1 KB");
        assert_eq!(bytes(1536 * 1024), "2 MB");
        assert_eq!(bytes(532 * 1024 * 1024), "532 MB");
        assert_eq!(bytes(1_288_490_189), "1.2 GB");
        assert_eq!(bytes(10 * 1024 * 1024 * 1024), "10.0 GB");
        assert_eq!(rss(91 * 1024 * 1024), "91 MB");
        let gib = 1u64 << 30;
        assert_eq!(gb_pair(51 * gib, 79 * gib, 10.0), "51 / 79 GB");
        assert_eq!(gb_pair(164 * gib, 200 * gib, 10.0), "164 / 200 GB");
        assert_eq!(gb_pair(200 << 20, gib, 10.0), "0.2 / 1 GB", "a whole number of gigabytes has no decimal");
        assert_eq!(gb_pair(9830 << 20, 15_974 << 20, 100.0), "9.6 / 15.6 GB", "memory has a decimal up to 100 GB");
        assert_eq!(gb_pair(10 * gib, 16 * gib, 100.0), "10 / 16 GB");
        assert_eq!(gb_pair(50 << 20, 80 << 20, 10.0), "50 / 80 MB");
        assert_eq!(gigabytes(79 * 1024 * 1024 * 1024), "79 GB");
        assert_eq!(gigabytes(1_073_741_824), "1.0 GB");
        assert_eq!(gigabytes(200 * 1024 * 1024), "0.2 GB");
        assert_eq!(gigabytes(50 * 1024 * 1024), "50 MB", "below a tenth of a gigabyte the size is in megabytes");
    }

    #[test]
    fn rates_share_one_unit() {
        assert_eq!(rate_pair(0.0, 0.0), ("0".into(), "0".into(), "B/s"));
        assert_eq!(rate_pair(131.0, 689.0), ("131".into(), "689".into(), "B/s"));
        // The larger of the two decides the unit, and the smaller is written in it.
        assert_eq!(rate_pair(2048.0, 5.0 * 1024.0), ("2.0".into(), "5.0".into(), "KB/s"));
        assert_eq!(rate_pair(0.4 * 1_048_576.0, 2.4 * 1_048_576.0), ("0.4".into(), "2.4".into(), "MB/s"));
        assert_eq!(rate_pair(300.0, 3.0 * 1_048_576.0), ("0.0".into(), "3.0".into(), "MB/s"));
        assert_eq!(rate_pair(150.0 * 1_048_576.0, 1.0), ("150".into(), "0.0".into(), "MB/s"));
        assert_eq!(rate_pair(3.0 * 1_073_741_824.0, 0.0).2, "GB/s");
        assert_eq!(rate_pair(-5.0, -1.0), ("0".into(), "0".into(), "B/s"), "a negative rate is nothing");
    }

    #[test]
    fn percentages() {
        assert_eq!(percent_whole(Some(18.6)), "19");
        assert_eq!(percent_whole(Some(0.4)), "0");
        assert_eq!(percent_whole(Some(250.0)), "100", "a card never says more than 100");
        assert_eq!(percent_whole(None), "—", "a placeholder, not a zero");
        assert_eq!(percent_cell(Some(13.04)), "13.0");
        assert_eq!(percent_cell(Some(143.2)), "143.2", "a process with threads can use more than one core");
        assert_eq!(percent_cell(None), "—");
    }

    #[test]
    fn docker_sizes() {
        assert_eq!(decimal_size(3_200_000_000), "3.2 GB");
        assert_eq!(decimal_size(1_240_000_000), "1.2 GB");
        assert_eq!(decimal_size(29_000_000), "29 MB");
        assert_eq!(decimal_size(7_800_000), "7.8 MB");
        assert_eq!(decimal_size(812_000), "812 kB");
        assert_eq!(decimal_size(512), "512 B");
        assert_eq!(bytes_short(1 << 30), "1 GB");
        assert_eq!(bytes_short(256 << 20), "256 MB");
        assert_eq!(bytes_short(3 << 29), "1.5 GB");
        assert_eq!(bytes_short(33_554_432), "32 MB");
        assert_eq!(bytes_short(512 * 1024), "512 KB");
    }

    #[test]
    fn durations_and_load() {
        assert_eq!(uptime(41.0 * 86_400.0 + 5.0), "41 天");
        assert_eq!(uptime(3.0 * 3600.0 + 12.0 * 60.0 + 9.0), "3 小时 12 分");
        assert_eq!(uptime(12.0 * 60.0), "12 分钟");
        assert_eq!(uptime(30.0), "刚启动");
        assert_eq!(uptime(-4.0), "刚启动");
        assert_eq!(load(0.42, 0.38, 0.311), "0.42 0.38 0.31");
    }
}
