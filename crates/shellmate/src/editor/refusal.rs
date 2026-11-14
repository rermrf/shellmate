//! Files the editor will not open, and what to offer instead.

use shellmate_sftp::SftpError;

/// Whether `error` is the editor declining a file (as opposed to the server failing): a dialog
/// says why, with the reason M1 wrote.
pub fn is_refusal(error: &SftpError) -> bool {
    matches!(error, SftpError::TooLarge { .. } | SftpError::Binary | SftpError::NotUtf8 | SftpError::NotAFile { .. })
}

/// Whether the dialog offers 下载: the file is there and fine, just not for the editor. A pipe or a
/// device is not downloaded either.
pub fn offers_download(error: &SftpError) -> bool {
    matches!(error, SftpError::TooLarge { .. } | SftpError::Binary | SftpError::NotUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_editor_declines_is_a_refusal_and_files_it_could_not_read_are_not() {
        for refusal in [
            SftpError::TooLarge { size: 9 << 20 },
            SftpError::Binary,
            SftpError::NotUtf8,
            SftpError::NotAFile { path: "/dev/null".into() },
        ] {
            assert!(is_refusal(&refusal), "{refusal:?}");
        }
        for failure in [
            SftpError::NotFound { path: "/x".into() },
            SftpError::PermissionDenied { path: "/x".into() },
            SftpError::Timeout,
            SftpError::IsADirectory { path: "/x".into() },
        ] {
            assert!(!is_refusal(&failure), "{failure:?}");
        }
    }

    #[test]
    fn a_download_is_offered_for_files_that_are_fine_but_not_for_the_editor() {
        assert!(offers_download(&SftpError::Binary));
        assert!(offers_download(&SftpError::NotUtf8));
        assert!(offers_download(&SftpError::TooLarge { size: 1 << 30 }));
        assert!(!offers_download(&SftpError::NotAFile { path: "/dev/null".into() }));
    }

    #[test]
    fn the_reasons_are_the_ones_the_sftp_layer_wrote() {
        assert_eq!(SftpError::Binary.to_string(), "这是二进制文件，不能作为文本打开，请下载后查看");
        assert_eq!(SftpError::NotUtf8.to_string(), "文件不是 UTF-8 编码的文本，不能打开，请下载后查看");
        assert!(SftpError::TooLarge { size: 6 << 20 }.to_string().contains("5.0 MB"));
    }
}
