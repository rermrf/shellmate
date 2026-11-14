//! Names of transferred files: numbered copies and temporary files.

/// `report.pdf` → `report (2).pdf`; without an extension (or for a directory, or a dotfile such
/// as `.bashrc`) → `report (2)`.
pub fn numbered(name: &str, n: u32, keep_extension: bool) -> String {
    match split_extension(name).filter(|_| keep_extension) {
        Some((stem, extension)) => format!("{stem} ({n}).{extension}"),
        None => format!("{name} ({n})"),
    }
}

/// The name itself, then `name (1)`, `name (2)`, … : the names to try until one is free.
pub fn candidates(name: &str, keep_extension: bool) -> impl Iterator<Item = String> {
    let name = name.to_string();
    std::iter::once(name.clone()).chain((1..=MAX_NUMBER).map(move |n| numbered(&name, n, keep_extension)))
}

/// The most numbered copies tried before giving up.
pub const MAX_NUMBER: u32 = 9999;

/// The first candidate that `taken` does not claim.
pub fn first_free(name: &str, keep_extension: bool, mut taken: impl FnMut(&str) -> bool) -> Option<String> {
    candidates(name, keep_extension).find(|candidate| !taken(candidate))
}

/// `(stem, extension)` of a file name; `None` if there is no extension (`Makefile`, `.bashrc`,
/// `archive.`).
fn split_extension(name: &str) -> Option<(&str, &str)> {
    let dot = name.rfind('.')?;
    let (stem, extension) = (&name[..dot], &name[dot + 1..]);
    (!stem.is_empty() && !extension.is_empty()).then_some((stem, extension))
}

/// The temporary name of a file being downloaded.
pub fn download_temp_name(name: &str) -> String {
    format!("{name}.shellmate-download")
}

/// The temporary name of a file being uploaded: next to the target, so the final rename stays on
/// one filesystem.
pub fn upload_temp_path(target: &str) -> String {
    crate::text::sibling(target, |name| format!(".{name}.shellmate-upload"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_copies_keep_the_extension() {
        assert_eq!(numbered("report.pdf", 1, true), "report (1).pdf");
        assert_eq!(numbered("report.pdf", 12, true), "report (12).pdf");
        assert_eq!(numbered("archive.tar.gz", 1, true), "archive.tar (1).gz");
        assert_eq!(numbered("报告 终稿.docx", 2, true), "报告 终稿 (2).docx");
    }

    #[test]
    fn without_an_extension_the_number_goes_last() {
        assert_eq!(numbered("Makefile", 1, true), "Makefile (1)");
        assert_eq!(numbered(".bashrc", 1, true), ".bashrc (1)");
        assert_eq!(numbered("archive.", 1, true), "archive. (1)");
        assert_eq!(numbered("photos", 3, false), "photos (3)");
        assert_eq!(numbered("photos.2024", 1, false), "photos.2024 (1)", "a directory has no extension");
        assert_eq!(numbered(".hidden.txt", 1, true), ".hidden (1).txt");
    }

    #[test]
    fn candidates_start_with_the_plain_name() {
        let names: Vec<String> = candidates("a.txt", true).take(4).collect();
        assert_eq!(names, ["a.txt", "a (1).txt", "a (2).txt", "a (3).txt"]);
    }

    #[test]
    fn the_first_free_name_is_found() {
        let taken = ["a.txt", "a (1).txt", "a (2).txt"];
        assert_eq!(first_free("a.txt", true, |n| taken.contains(&n)).as_deref(), Some("a (3).txt"));
        assert_eq!(first_free("b.txt", true, |n| taken.contains(&n)).as_deref(), Some("b.txt"));
        assert_eq!(first_free("a.txt", true, |_| true), None, "everything taken");
    }

    #[test]
    fn temporary_names_are_documented() {
        assert_eq!(download_temp_name("movie.mkv"), "movie.mkv.shellmate-download");
        assert_eq!(upload_temp_path("/srv/data/movie.mkv"), "/srv/data/.movie.mkv.shellmate-upload");
        assert_eq!(upload_temp_path("/文档/报告.md"), "/文档/.报告.md.shellmate-upload");
    }
}
