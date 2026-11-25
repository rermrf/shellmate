//! Child transcript files share the P11 conversation output directory and retention sweep.
use shellmate_core::ai_output;
use std::{
    io::{self, Read, Write},
    path::Path,
};
pub fn write(
    root: &Path,
    conversation: &str,
    id: &str,
    entries: &[shellmate_ai::Entry],
) -> io::Result<()> {
    let path = ai_output::path(root, conversation, id)?.with_extension("json");
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(temp.as_file_mut(), entries).map_err(io::Error::other)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub fn read(root: &Path, conversation: &str, id: &str) -> Option<Vec<shellmate_ai::Entry>> {
    let path = ai_output::path(root, conversation, id)
        .ok()?
        .with_extension("json");
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 64 * 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
