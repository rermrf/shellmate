use std::io;
use std::process::Stdio;

use crate::FollowEvent;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

async fn line(reader: &mut (impl AsyncBufRead + Unpin)) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            break;
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(buffer.len());
        bytes.extend_from_slice(&buffer[..count.min(4096 - bytes.len())]);
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    Ok(Some(
        String::from_utf8_lossy(bytes.strip_suffix(b"\r").unwrap_or(&bytes)).into_owned(),
    ))
}

pub(crate) async fn follow_file(path: &str, events: &async_channel::Sender<FollowEvent>) -> io::Result<()> {
    let mut child = tokio::process::Command::new("/usr/bin/tail")
        .args(["-n", "50", "-F", path])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    while let Some(line) = line(&mut reader).await? {
        if events.send(FollowEvent::Line(line)).await.is_err() {
            return Ok(());
        }
    }
    let status = child.wait().await?;
    let why = if status.success() {
        String::new()
    } else {
        format!("无法跟随日志：{status}")
    };
    let _ = events.send(FollowEvent::Ended(why)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_follower_bounds_long_lines_and_survives_invalid_utf8() {
        let mut input = vec![b'x'; 20000];
        input.extend_from_slice(b"\n\xff\r\n\nlast");
        let mut reader = BufReader::with_capacity(32, input.as_slice());
        for expected in ["x".repeat(4096), "�".into(), "".into(), "last".into()] {
            assert_eq!(line(&mut reader).await.unwrap(), Some(expected));
        }
        assert_eq!(line(&mut reader).await.unwrap(), None);
    }
}
