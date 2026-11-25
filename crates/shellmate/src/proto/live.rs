pub const SCENES: &[&str] = &[
    "ai-live-queue",
    "ai-live-waiting",
    "ai-live-approval",
    "ai-live-restored",
    "ai-live-target",
    "ai-live-btw",
];
pub fn base(scene: &str) -> Option<&'static str> {
    if !SCENES.contains(&scene) {
        return None;
    }
    Some(match scene {
        "ai-live-queue" => "ai-agent-run",
        "ai-live-waiting" => "ai-subagent-running",
        "ai-live-approval" => "ai-subagent-approval",
        "ai-live-restored" | "ai-live-target" => "ai-subagent-done",
        "ai-live-btw" => "ai-agent-run",
        _ => return None,
    })
}
