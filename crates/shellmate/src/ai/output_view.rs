//! Display-only projection of the spooled tool result. Durable/wire text stays intact.
pub struct OutputView<'a> {
    pub preview: &'a str,
    pub saved: Option<String>,
    pub prose: bool,
}

#[cfg(test)]
pub fn display(output: &str) -> OutputView<'_> {
    display_step(output, false)
}
pub fn display_step(output: &str, status_is_prose: bool) -> OutputView<'_> {
    let original = || OutputView {
        preview: output,
        saved: None,
        prose: status_is_prose,
    };
    let Some((header, preview)) = output.split_once('\n') else {
        return original();
    };
    let Some((id, rest)) = header
        .strip_prefix("输出 id ")
        .and_then(|s| s.split_once(" · 总计 "))
    else {
        return original();
    };
    let Some((bytes, status)) = rest.split_once(" 字节 · 退出码 ") else {
        return original();
    };
    let Ok(bytes) = bytes.parse::<u64>() else {
        return original();
    };
    let exit = status.split(" · ").next().unwrap_or_default();
    if id.is_empty() || (exit != "未知" && exit.parse::<i32>().is_err()) {
        return original();
    }
    let size = if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.)
    } else {
        format!("{bytes} 字节")
    };
    OutputView {
        preview: preview
            .strip_suffix("\n完整输出已保存，用 read_output 查看或搜索")
            .unwrap_or(preview),
        saved: Some(format!("完整输出 {size} 已保存 · {id}")),
        prose: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{display, display_step};
    #[test]
    fn spooled_preview_keeps_failure_marker_and_wire_is_untouched() {
        let wire = "输出 id output-1 · 总计 1048576 字节 · 退出码 0 · 1500 ms\npreview\n完整输出已保存，用 read_output 查看或搜索";
        let view = display(wire);
        assert_eq!(view.preview, "preview");
        assert_eq!(
            view.saved.as_deref(),
            Some("完整输出 1.0 MB 已保存 · output-1")
        );
        assert!(wire.contains("退出码 0 · 1500 ms"));
        let failure = "输出 id output-2 · 总计 86024 字节 · 退出码 101\nfirst\n[省略中间输出]\n[stderr] failed";
        let view = display(failure);
        assert_eq!(view.preview, "first\n[省略中间输出]\n[stderr] failed");
        assert_eq!(
            view.saved.as_deref(),
            Some("完整输出 84.0 KB 已保存 · output-2")
        );
    }
    #[test]
    fn promotion_and_status_are_prose_but_captured_output_stays_preformatted() {
        let text = "命令在 120 秒内未完成，已转到后台继续运行 · 任务 #1 · 输出 id output-1";
        assert!(display_step(text, true).prose);
        assert!(!display_step("command stdout", false).prose);
        assert!(!display_step("输出 id x · 总计 12 字节 · 退出码 0\nstdout", true).prose);
    }
    #[test]
    fn ordinary_legacy_and_malformed_outputs_are_unchanged() {
        for output in [
            "",
            "标准输出：\nok\n标准错误：\n",
            "输出 id example\nunchanged",
            "输出 id x · 总计 oops 字节 · 退出码 0\nbody",
        ] {
            let view = display(output);
            assert_eq!(view.preview, output);
            assert!(view.saved.is_none());
        }
    }
}
