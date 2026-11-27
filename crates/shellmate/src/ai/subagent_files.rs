//! Child transcript files share the P11 conversation output directory and retention sweep.
use shellmate_core::ai_output;
use std::{
    io::{self, Read, Write},
    path::Path,
};
static CHECKPOINTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    pub entries: Vec<shellmate_ai::Entry>,
    pub inbox: shellmate_ai::interaction::Snapshot,
}
pub fn checkpoint(
    root: &Path,
    conversation: &str,
    id: &str,
    entries: Option<&[shellmate_ai::Entry]>,
    mut inbox: shellmate_ai::interaction::Snapshot,
) -> io::Result<()> {
    let _guard = CHECKPOINTS.lock().unwrap();
    let path = ai_output::path(root, conversation, id)?.with_extension("checkpoint.json");
    let entries = entries
        .map(|e| e.to_vec())
        .or_else(|| read_checkpoint(root, conversation, id).map(|c| c.entries))
        .unwrap_or_default();
    if let Some(old) = read_checkpoint(root, conversation, id) {
        merge_question_answers(&mut inbox, &old.inbox, &entries);
    }
    write_checkpoint(&path, &Checkpoint { entries, inbox })
}
/// Message arrival appends to the last durable inbox. A worker may already have
/// claimed an older input without having saved its corresponding user entry yet.
pub fn append_input(
    root: &Path,
    conversation: &str,
    id: &str,
    input: &shellmate_ai::interaction::Input,
) -> io::Result<()> {
    let _guard = CHECKPOINTS.lock().unwrap();
    let path = ai_output::path(root, conversation, id)?.with_extension("checkpoint.json");
    let mut saved = read_checkpoint(root, conversation, id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "子任务恢复记录不可用"))?;
    if !saved.inbox.inputs.iter().any(|i| i.id == input.id) {
        saved.inbox.inputs.push_back(input.clone());
    }
    write_checkpoint(&path, &saved)
}
/// Preserve committed answers across a worker checkpoint that captured its inbox earlier.
fn merge_question_answers(
    inbox: &mut shellmate_ai::interaction::Snapshot,
    old: &shellmate_ai::interaction::Snapshot,
    entries: &[shellmate_ai::Entry],
) {
    for q in &old.questions {
        if let Some(current) = inbox.questions.iter_mut().find(|r| r.id == q.id) {
            if current.response.is_none() {
                current.response = q.response.clone();
            }
        } else {
            inbox.questions.push(q.clone());
        }
    }
    for input in &old.inputs {
        if let Some(id) = &input.question_id
            && !entries.iter().any(
                |e| matches!(e,shellmate_ai::Entry::QuestionAnswer{request_id,..}if request_id==id),
            )
            && !inbox.inputs.iter().any(|i| i.id == input.id)
        {
            inbox.inputs.push_back(input.clone());
        }
    }
    inbox.inputs.retain(|input| {
        input.question_id.as_ref().is_none_or(|id| {
            !entries.iter().any(
                |e| matches!(e,shellmate_ai::Entry::QuestionAnswer{request_id,..}if request_id==id),
            )
        })
    });
}
pub fn save_question_answers(
    root: &Path,
    conversation: &str,
    id: &str,
    inbox: &shellmate_ai::interaction::Snapshot,
) -> io::Result<()> {
    let _guard = CHECKPOINTS.lock().unwrap();
    let path = ai_output::path(root, conversation, id)?.with_extension("checkpoint.json");
    let mut saved = read_checkpoint(root, conversation, id)
        .ok_or_else(|| io::Error::other("子任务恢复记录不可用"))?;
    merge_question_answers(&mut saved.inbox, inbox, &saved.entries);
    write_checkpoint(&path, &saved)
}
pub fn save_question_draft(
    root: &Path,
    conversation: &str,
    child: &str,
    id: &str,
    draft: std::collections::BTreeMap<String, shellmate_ai::questions::Answer>,
) -> io::Result<()> {
    let _guard = CHECKPOINTS.lock().unwrap();
    let path = ai_output::path(root, conversation, child)?.with_extension("checkpoint.json");
    let mut saved = read_checkpoint(root, conversation, child)
        .ok_or_else(|| io::Error::other("子任务恢复记录不可用"))?;
    if let Some(q) = saved
        .inbox
        .questions
        .iter_mut()
        .find(|q| q.id == id && q.response.is_none())
    {
        q.draft = draft;
    }
    write_checkpoint(&path, &saved)
}
fn write_checkpoint(path: &Path, checkpoint: &Checkpoint) -> io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(file.as_file_mut(), checkpoint).map_err(io::Error::other)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub fn read_checkpoint(root: &Path, conversation: &str, id: &str) -> Option<Checkpoint> {
    let path = ai_output::path(root, conversation, id)
        .ok()?
        .with_extension("checkpoint.json");
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = vec![];
    file.take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 64 * 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
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

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_ai::interaction::Control;

    #[test]
    fn arriving_input_preserves_claims_from_the_last_durable_transcript() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("ai-outputs/conversation");
        std::fs::create_dir_all(&dir).unwrap();
        let control = Control::default();
        control.resume();
        control.enqueue(
            "first".into(),
            "first instruction".into(),
            Default::default(),
        );
        checkpoint(
            root.path(),
            "conversation",
            "agent-1",
            Some(&[]),
            control.snapshot(),
        )
        .unwrap();
        // The worker has claimed this message but its user entry is not saved yet.
        control.claim();
        control
            .enqueue_with(
                "second".into(),
                "second instruction".into(),
                Default::default(),
                |input| append_input(root.path(), "conversation", "agent-1", input),
            )
            .unwrap();
        let saved = read_checkpoint(root.path(), "conversation", "agent-1").unwrap();
        assert_eq!(saved.inbox.inputs.len(), 2);
        assert!(saved.inbox.inputs.iter().all(|i| !i.claimed));
        let entries = vec![shellmate_ai::Entry::User {
            attachments: Vec::new(),
            sent_at_ms: None,
            origin: None,
            text: "first instruction".into(),
            context: Default::default(),
        }];
        checkpoint(
            root.path(),
            "conversation",
            "agent-1",
            Some(&entries),
            control.snapshot(),
        )
        .unwrap();
        control.acknowledge(&["first".into()]);
        control
            .enqueue_with(
                "third".into(),
                "third instruction".into(),
                Default::default(),
                |input| append_input(root.path(), "conversation", "agent-1", input),
            )
            .unwrap();
        let saved = read_checkpoint(root.path(), "conversation", "agent-1").unwrap();
        assert_eq!(saved.entries.len(), 1);
        assert!(saved.inbox.inputs[0].claimed);
        assert_eq!(
            saved
                .inbox
                .inputs
                .iter()
                .filter(|i| !i.claimed)
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["second", "third"]
        );
    }
}

#[cfg(test)]
mod question_tests {
    use super::*;
    #[test]
    fn stale_checkpoint_cannot_erase_a_committed_answer_or_deliver_it_twice() {
        use shellmate_ai::{
            interaction::Control,
            questions::{Answer, Mode, Pending, Question, Request, Response, Status},
        };
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ai_output::directory(root.path(), "conversation").unwrap())
            .unwrap();
        let c = Control::default();
        let request = Request {
            questions: vec![Question {
                id: "q".into(),
                question: "如何继续？".into(),
                options: vec![],
                multi_select: false,
            }],
            mode: Mode::Wait,
        };
        c.register_question(Pending::new(
            "request".into(),
            shellmate_ai::LoggedCall {
                id: "call".into(),
                name: "ask_user".into(),
                args_json: serde_json::to_value(&request).unwrap(),
            },
            request,
            Default::default(),
        ))
        .unwrap();
        let stale = c.snapshot();
        checkpoint(
            root.path(),
            "conversation",
            "agent",
            Some(&[]),
            stale.clone(),
        )
        .unwrap();
        c.answer_question(
            "request",
            Response {
                status: Status::Answered,
                answers: std::collections::BTreeMap::from([(
                    "q".into(),
                    Answer {
                        selected: vec![],
                        text: "继续检查".into(),
                    },
                )]),
            },
            |snapshot| {
                save_question_answers(root.path(), "conversation", "agent", snapshot)
                    .map_err(|e| e.to_string())
            },
        )
        .unwrap();
        checkpoint(
            root.path(),
            "conversation",
            "agent",
            Some(&[]),
            stale.clone(),
        )
        .unwrap();
        let saved = read_checkpoint(root.path(), "conversation", "agent").unwrap();
        assert!(saved.inbox.questions[0].response.is_some());
        assert_eq!(saved.inbox.inputs.len(), 1);
        c.resume();
        let input = c.claim_first().unwrap();
        let entries = vec![c.input_entry(&input)];
        checkpoint(root.path(), "conversation", "agent", Some(&entries), stale).unwrap();
        let saved = read_checkpoint(root.path(), "conversation", "agent").unwrap();
        assert!(saved.inbox.inputs.is_empty());
        assert_eq!(saved.entries, entries);
    }
}
