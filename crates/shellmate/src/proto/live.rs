pub const SCENES: &[&str] = &[
    "ai-multi-conversations","ai-multi-targets","ai-multi-compare",
    "ai-tools-unified",
    "ai-tools-expanded",
    "ai-web-search",
    "ai-web-fetch",
    "ai-web-fallback",
    "ai-web-error",
    "ai-child-thread",
    "ai-child-running",
    "ai-child-approval",
    "ai-child-restored",
    "ai-child-long-prompt",
    "ai-live-queue",
    "ai-live-waiting",
    "ai-service-child",
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
        "ai-multi-conversations"|"ai-multi-targets"|"ai-multi-compare"=>"ai-chat",
        "ai-tools-unified" | "ai-tools-expanded" => "ai-chat",
        "ai-web-search" | "ai-web-fetch" | "ai-web-fallback" | "ai-web-error" => "ai-chat",
        "ai-child-thread"
        | "ai-child-running"
        | "ai-child-approval"
        | "ai-child-restored"
        | "ai-child-long-prompt" => "ai-subagent-done",
        "ai-live-queue" => "ai-agent-run",
        "ai-service-child" => "ai-output-running",
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
