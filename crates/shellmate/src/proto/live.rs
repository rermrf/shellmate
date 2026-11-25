pub const SCENES: &[&str] = &[
    "ai-live-queue",
    "ai-live-waiting",
    "ai-live-approval",
    "ai-live-restored",
    "ai-live-target",
    "ai-live-btw",
    "ai-timing-running",
    "ai-timing-waiting",
    "ai-timing-done",
    "ai-timing-paused",
    "ai-timing-interrupted",
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
        "ai-timing-waiting" => "ai-subagent-running",
        "ai-timing-running" => "ai-agent-run",
        "ai-timing-done" | "ai-timing-paused" | "ai-timing-interrupted" => "ai-chat",
        _ => return None,
    })
}
