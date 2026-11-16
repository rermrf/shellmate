//! Pure sizing rules for real server values. JetBrains Mono uses a 0.6 em digit advance.

/// Linux pid_max is 4_194_304; reserve seven digits even when today's sample has small PIDs.
pub fn pid_width(max_pid: u32, digit: f32) -> f32 {
    let digits = max_pid.max(4_194_304).ilog10() + 1;
    (digits as f32 * digit).ceil() + 8.
}

/// A numeric value and its unit are indivisible. Account for every formatted character.
pub fn numeric_width<'a>(values: impl IntoIterator<Item = &'a str>, digit: f32, minimum: f32) -> f32 {
    values.into_iter().map(|v| (v.chars().count() as f32 * digit).ceil() + 4.).fold(minimum, f32::max)
}

/// Retain the final path component and a recognizable prefix, without splitting UTF-8.
/// The full path is always available in the UI tooltip and breadcrumb picker.
pub fn middle_ellipsis(text: &str, budget: usize) -> String {
    let chars: Vec<_> = text.chars().collect();
    if chars.len() <= budget {
        return text.into();
    }
    if budget == 0 {
        return String::new();
    }
    if budget == 1 {
        return "…".into();
    }
    let last = text.rsplit('/').next().unwrap_or(text).chars().count();
    let tail = last.min((budget - 1) * 2 / 3).max((budget - 1) / 2);
    let mut head = budget - 1 - tail;
    let mut tail_start = chars.len() - tail;
    // Without adding a Unicode segmentation dependency, conservatively keep non-ASCII runs
    // intact (including combining marks and emoji joiners) and cut only between ASCII scalars.
    // Tooltips and the breadcrumb menu retain the original, unabridged path.
    let safe = |at: usize| at == 0 || at == chars.len() || (chars[at - 1].is_ascii() && chars[at].is_ascii());
    while !safe(head) {
        head -= 1;
    }
    while !safe(tail_start) {
        tail_start += 1;
    }
    chars[..head].iter().chain(std::iter::once(&'…')).chain(chars[tail_start..].iter()).collect()
}

/// Decisions use the measured panel width, never the OS window width or intrinsic text width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Responsive {
    pub summary_columns: usize,
    pub overview_stacked: bool,
    pub header_stacked: bool,
    pub process_rss: bool,
    pub process_user: bool,
    pub services_stacked: bool,
    pub service_detail_width: f32,
    pub service_extra_columns: bool,
    pub docker_compact: bool,
    pub files_inline_details: bool,
    pub forward_stacked_stats: bool,
    pub files_toolbar_stacked: bool,
}
impl Responsive {
    pub fn at(width: f32) -> Self {
        Self {
            summary_columns: if width >= 1000. { 4 } else { 2 },
            overview_stacked: width < 1000.,
            header_stacked: width < 1000.,
            process_rss: width >= 1000.,
            process_user: width >= 850.,
            services_stacked: width < 680.,
            service_detail_width: if width >= 1000. { 400. } else { (width * 0.44).clamp(300., 360.) },
            service_extra_columns: width >= 1000.,
            docker_compact: width < 1000.,
            files_inline_details: width >= 1100.,
            forward_stacked_stats: width < 900.,
            files_toolbar_stacked: width < 900.,
        }
    }
    pub fn summary_width(self, width: f32) -> f32 {
        ((width - 10. * (self.summary_columns - 1) as f32) / self.summary_columns as f32).max(0.)
    }
}

/// Keep short prototype port mappings inline only when their complete chips fit.
pub fn inline_container_ports(width: f32, cpu_width: f32, memory_width: f32, labels: &[&str]) -> bool {
    if Responsive::at(width).docker_compact || labels.len() > 2 {
        return false;
    }
    let name = if width >= 1100. { 241. } else { width * 0.20 };
    let status = if width >= 1100. { 199. } else { width * 0.16 };
    let available = (width - 40. - 8. - name - status - cpu_width - memory_width - 12. - 220. - 50.).max(0.);
    let required = labels.iter().map(|label| label.chars().count() as f32 * 6.9 + 16.).sum::<f32>()
        + labels.len().saturating_sub(1) as f32 * 6.;
    required <= available
}

/// After reserving the name, units and actions, the progress bar is the first flexible part.
pub fn transfer_progress_width(width: f32, size_width: f32) -> f32 {
    (width - 32. - 28. - size_width - 56. - 52. - 60. - 220.).max(0.).min(width * 0.32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn responsive_breakpoints_and_viewport_budgets() {
        for width in [480., 679., 680., 804., 849., 850., 999., 1000., 1099., 1100., 1144., 1480.] {
            let r = Responsive::at(width);
            assert!(
                (r.summary_width(width) * r.summary_columns as f32 + 10. * (r.summary_columns - 1) as f32 - width)
                    .abs()
                    < 0.01
            );
            assert_eq!(r.summary_columns, if width >= 1000. { 4 } else { 2 });
            assert_eq!(r.process_rss, width >= 1000.);
            assert_eq!(r.process_user, width >= 850.);
            assert_eq!(r.overview_stacked, width < 1000.);
            assert_eq!(r.header_stacked, width < 1000.);
            assert_eq!(r.files_inline_details, width >= 1100.);
            assert_eq!(r.docker_compact, width < 1000.);
            assert_eq!(r.forward_stacked_stats, width < 900.);
            assert_eq!(r.files_toolbar_stacked, width < 900.);
            let fixed =
                72. + 56. + 40. + if r.process_rss { 130. + 96. } else { 70. } + if r.process_user { 128. } else { 0. };
            let gaps = 12. * (4. + u8::from(r.process_rss) as f32 + u8::from(r.process_user) as f32);
            assert!(width - fixed - gaps - 56. >= 100., "command column remains usable at {width}");
            if !r.services_stacked {
                assert!(width - r.service_detail_width - 10. >= 300.);
            }
        }
        let prototype = Responsive::at(1144.);
        assert_eq!(prototype.service_detail_width, 400.);
        assert!(prototype.files_inline_details && prototype.process_rss && prototype.process_user);
    }

    #[test]
    fn ports_move_as_whole_chips_when_the_row_runs_out_of_space() {
        assert!(inline_container_ports(1144., 52., 120., &["127.0.0.1:3000→3000"]));
        assert!(!inline_container_ports(804., 52., 120., &["80/tcp"]));
        assert!(!inline_container_ports(1144., 52., 120., &["[2001:db8:abcd:1234::100]:60000→8000"]));
        assert!(!inline_container_ports(1480., 52., 120., &["80/tcp", "443/tcp", "8080/tcp"]));
    }

    #[test]
    fn transfer_bar_yields_before_names_sizes_or_actions() {
        let size = 172.;
        assert_eq!(transfer_progress_width(700., size), 80.);
        assert_eq!(transfer_progress_width(600., size), 0.);
        for width in [684., 804., 1144., 1480.] {
            let bar = transfer_progress_width(width, size);
            assert!(width - 32. - 28. - size - 56. - 52. - 60. - bar >= 180.);
        }
    }
    #[test]
    fn pid_width_covers_linux_max_and_larger_unsigned_ids() {
        for pid in [1, 186183, 775347, 4194304, u32::MAX] {
            let width = pid_width(pid, 7.8);
            assert!(width >= pid.to_string().len() as f32 * 7.8 + 8.);
        }
        assert_eq!(pid_width(1, 7.8), pid_width(4194304, 7.8));
    }
    #[test]
    fn numbers_keep_units_and_widest_value_in_the_column() {
        let values = ["1.2 TB", "12.5 GB/s", "123456 B/s", "18446744073709551615 B"];
        let width = numeric_width(values, 7.8, 40.);
        for value in values {
            assert!(width >= value.len() as f32 * 7.8 + 4.);
        }
    }
    #[test]
    fn ellipsis_retains_ends_and_never_slices_a_unicode_character() {
        assert_eq!(middle_ellipsis("/var/www", 20), "/var/www");
        for path in ["e\u{301}e\u{301}e\u{301}", "👩‍💻👩‍💻👩‍💻"] {
            assert_eq!(middle_ellipsis(path, 4), "…");
        }
        for path in [
            "/var/lib/containers/releases/production/config.yaml",
            "/主机/生产环境/发布目录/配置文件.yaml",
            "👩‍💻/部署/配置",
        ] {
            for budget in 0..20 {
                let result = middle_ellipsis(path, budget);
                assert!(result.chars().count() <= budget);
                if path.chars().count() > budget && budget > 1 {
                    assert!(result.contains('…'));
                    assert!(path.ends_with(result.split('…').next_back().unwrap()));
                }
            }
        }
    }
}
