pub mod panel;

pub fn recording_label(seconds: Option<f64>) -> String {
    seconds.map_or_else(
        || "● 录制".into(),
        |s| format!("● REC {} · .cast", shellmate_term::archive::mmss(s)),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn rec_label_includes_dot_elapsed_and_cast_extension() {
        assert_eq!(super::recording_label(None), "● 录制");
        assert_eq!(super::recording_label(Some(0.)), "● REC 00:00 · .cast");
        assert_eq!(super::recording_label(Some(89.9)), "● REC 01:29 · .cast");
    }
}
