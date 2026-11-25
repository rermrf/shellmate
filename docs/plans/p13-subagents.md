# P13 AI subagents (Claude Code style)

## Goal
Today one drawer runs one agent with one context. A long investigation, such as 「为什么 502」 across nginx, application and system logs, fills the main context with output that is only needed to reach a conclusion. Independent checks also run strictly one after another.

P13 lets the main agent delegate a self-contained task to a **subagent**:
- The subagent has its own context window, runs its own tool loop on the **same host** in the background, and hands back **only a final report**.
- Several subagents can run at once in the background.

The design follows Claude Code (CC); Codex is a second reference.

P13 builds on:
- **P11** (`docs/plans/p11-agent-long-tasks.md`): the conversation log, compaction, background tasks, output spooling, `read_output` / `stop_task` and `/tasks`;
- **P12** (`docs/plans/p12-agent-memory.md`): instructions and memory.

## User decisions (2026-11-25)
| Topic | Decision |
|---|---|
| Scope | **Same host only.** Cross-host subagents get a separate discussion later. |
| Approvals in 每步审批 | A general subagent's approval cards **pop up as usual**, labelled with the subagent that asks. |
| Concurrency | Default **3** running subagents. A setting changes it. |
| Model | **Same as the main conversation by default.** The main agent may pass a **model and reasoning level** when the user asks for them in the prompt. |
| Custom subagent types | **Later, together with skills.** P13 ships two built-in types. |

## References (verified 2026-11-25)
**CC** ([sub-agents docs](https://code.claude.com/docs/en/sub-agents)):
- **Isolation:** own context window, system prompt, tool access and permissions. The subagent does not see the conversation history; it gets its prompt, the delegation message and CLAUDE.md. Only its final report returns; reports are scanned but not trusted.
- **Built-in types:**
  - Explore: read-only, fast;
  - Plan: read-only;
  - general-purpose: all tools.
- **Custom types:** `.claude/agents/*.md`, with frontmatter fields `name`, `description`, `tools`, `model`, `permissionMode`, `maxTurns`, …
- **Model resolution:** per-invocation parameter → definition → env → the parent's model.
- **Background:** background is the default for Claude-spawned subagents. Results arrive as completion notifications. Permission prompts surface in the main session, naming the subagent.
- **Concurrency:** 20 by default; going over fails with "Concurrent subagent limit reached".
- **Nesting:** depth 3 by default; configurable, and 1 disables it.
- **Turn limit:** hitting `maxTurns` marks the output as partial.
- **Transcripts:** stored separately, with 30-day cleanup.

**Codex:**
- The model drives it with `spawn_agent` / `wait` / `send_input` / `close_agent`; v2 adds path-addressed `followup_task`, `send_message`, `wait_agent`, `interrupt_agent` and `list_agents`.
- Custom agents: `.codex/agents/*.toml`.
- Results return to the parent only. Cost grows roughly linearly with the number of agents.

## Findings that drive the design (as of `9fab221`)
- **Sequential tool calls.** The agent loop sends `parallel_tool_calls(false)` (`shellmate-ai/src/agent.rs:605`), so tool calls run one at a time. Parallel work today comes only from P11 background commands.
- **Approval flow:** `ToolHost::run` (`shellmate/src/ai/host.rs:133`) classifies the command, calls `decide(mode, class)` and, if needed, sends `Ui::Approval` to the drawer, then waits.
  - The drawer keeps **one** pending approval: `state.active_step: Option<ActiveStep>` (`agent_ui.rs:247`).
- **One configured model:** `Config` holds one provider (OpenAI-compatible or Anthropic), one model and one API key (`shellmate-ai/src/config.rs:174`).
  - `ReasoningLevel` already has per-provider options: OpenAI-compatible has default, off, minimal, low, medium, high, xhigh, max; Anthropic has default, off, low, medium, high, max.
  - Context window, auto-compact window and output tokens are per provider + model settings.
- **Background task registry:** `shellmate/src/ai/tasks.rs` already provides ids, status, stop, output ids, completion notices (`take_notices`) and the `/tasks` list.
- **Terminal target:** it is one shared interactive surface. Telnet, mosh and serial sessions have no background channel (`Capabilities.targets()`).
- **SSH channels:** OpenSSH defaults `MaxSessions` to 10 channels per connection, and background commands already use exec channels.

## 1. The `agent` tool (main agent only)
**Schema:**
```json
{
  "description": "一句话标题，显示在卡片上（≤ 30 字）",
  "prompt": "完整、自包含的任务说明（子任务看不到对话历史）",
  "type": "explore | general",
  "run_in_background": false,
  "model": "可选：同一服务商下的模型 ID，仅在用户要求时填写",
  "reasoning": "可选：default | off | minimal | low | medium | high | xhigh | max（按服务商可用项）"
}
```

**Availability:**
- Offered only to the **main** agent; there is no nesting.
- Only when the session has a **background** execution target (SSH or local). Telnet, mosh and serial don't get it.

**Tool description (guidance for the model):**
- Use it for noisy, self-contained investigations, or for several independent checks in parallel (background).
- Don't use it for a single quick command.
- Write a self-contained prompt.
- Prefer `explore` for read-only diagnosis.
- Fill `model` / `reasoning` only when the user asked for them.
- The description names the current model and the reasoning ids valid for this provider.

**Validation errors** are returned to the model as a tool error:
- `model` empty, longer than 256 or containing control characters;
- `reasoning` not valid for the provider;
- the concurrency limit reached (「已有 3 个子任务在运行，请等待或停止其中一个」);
- no background target.

## 2. Subagent runtime (`shellmate-ai`, new `subagent.rs`)
**Context.** A fresh child `Conversation`. Its system prompt is:
- the execution prompt adapted for a subagent: same host and cwd; background target only; no user interaction; it can't ask questions;
- the safety rules, unchanged;
- instructions plus the memory index (P12 snapshot), **read-only**;
- the required report shape: 「结论 / 关键证据 / 执行过的变更 / 未完成或风险」.

It does not see the main conversation.

**Tools:**
- **`run_command`:** the target is fixed to **background**; the terminal is never offered.
- **`read_output`:** its own outputs only.
- **`stop_task`:** its own tasks only.
- **`memory`:** `view` only.
- **Not available:** `agent`, `propose_command` and memory writes.

**Types:**
- **`explore` (只读排查):** only commands classified `ReadOnly` run.
  - Anything else, red-line included, is **refused without asking**, returned as a tool error: 「只读子任务不能执行此命令：…；如需变更请写进报告」.
  - In 自动审批 or 完全访问 it therefore never interrupts the user. In 每步审批 its read-only commands still ask, per the user's mode.
- **`general` (通用):** the same `decide(mode, class)` as the main agent, under the host's current permission mode. Approvals go to the drawer labelled with the subagent (§4). Red-line commands always ask.

**Model:** `model` / `reasoning` override the main `Config` for this run only.
- They use the same provider, base URL and API key.
- The context window, auto-compact window and output tokens come from the per-model settings, then rig's catalog, then the P11 128K / 32K fallbacks.
- `reasoning` maps through `ReasoningLevel::for_provider`.

**Limits:**
- Inherits the 单轮步数上限 setting, tool-output length, command timeouts and compaction (tiers 1–2), all using the subagent's own window.
- Hitting the step limit ends the run with a **partial report**, marked 「（达到步数上限，结果不完整）」. A subagent has no interactive 继续.

**Report:**
- The report is the subagent's last assistant message.
- It is spooled like command output, so `read_output` can return all of it.
- The tool result holds the report inline, up to the tool-output limit, plus the output id and a one-line footer: 步数 · 耗时 · token 用量 · 模型.
- **Failures:**
  - provider errors are retried like the main agent;
  - a final failure returns 「子任务失败：<错误>」 and a partial report of what it ran;
  - cancellation returns 「子任务已停止」 and the partial report.

**Usage:** the subagent's input and output tokens are tracked separately and **not** counted in the main context ring. Only the report counts, as a tool result.

## 3. Foreground and background
**Foreground** (`run_in_background: false`):
- The main agent waits for the report. The drawer shows the subagent card updating live.
- The main 停止 stops it.

**Background** (`run_in_background: true`):
- **Start:** the subagent is registered in the P11 task registry as a task of kind **agent**, with id `agent-N`. The tool returns at once: 「子任务已在后台启动：agent-N（<description>）」.
- **Completion:** a notice like background commands, 「子任务 agent-N 已完成 / 失败 / 已停止」, injected at the next model request.
- **Control:**
  - `read_output agent-N` returns the report, or the progress so far while it runs;
  - `stop_task agent-N` stops it;
  - `/tasks` lists it with its type, status, steps and elapsed time.
- **Parallelism:** comes from background subagents. `parallel_tool_calls` stays off.
- **Lifetime:**
  - background subagents keep running after the main turn ends; their approvals still pop up in the drawer;
  - 新对话 (`/clear`), closing the session and stopping all tasks stop them, like background commands;
  - a disconnected session fails them with 「连接已断开」.

**Concurrency:**
- At most N running subagents; the setting 子任务并发上限 defaults to 3, range 1–8. Foreground subagents count too.
- Their background commands use exec channels like any background command.

## 4. Approvals with several requesters
`Ui::Approval` gains `source: Option<SubagentRef>` (id, description, type).

The drawer changes from one `active_step` to an **ordered list of pending approvals**:
- the main agent's card;
- any number of subagent cards.

**Card rules:**
- Each card is independent, in FIFO order, rendered in the conversation at the position of its requester.
- A subagent's card sits inside or under its subagent card.
- A subagent card shows a source line, 「子任务 · <description>」, and offers no 终端 target, since subagents run in the background only.
- 编辑 and 拒绝 work as today. A denial reason returns to the subagent as the tool result.
- Stopping a subagent cancels its pending approval, so the card disappears.

## 5. Drawer UI
The subagent card is a step card in the main conversation, in the same surface family as the approval card (card2 surface, same radius and padding).

**Header:**
- the 「子任务」 chip and a type chip (只读 / 通用);
- 「后台」 when the subagent runs in the background;
- the description.

**Status, on the right:**
- 运行中 · 3 步 · 12s
- 完成 · 8 步 · 1m02s
- 失败
- 已停止
- 达到步数上限

**Meta line:**
- the model and reasoning, only when overridden;
- the token usage.

**Collapsed:** the latest activity line, for example the command running now, plus 「展开」. Background cards also get 「停止」.

**Expanded:**
- the subagent's steps as compact rows (command, exit code, duration); each row expands to its output, as existing step cards do;
- the report, rendered as markdown, with a copy button.

Pending subagent approvals render inside the card, using the normal approval card.

**Other surfaces:**
- **`/tasks`:** subagents appear in the list with stop.
- **Settings:** under AI › 高级, add 「子任务并发上限」 (stepper 1–8, default 3), in the existing row style.

## 6. Persistence and replay
**Main log:**
- The `agent` tool call is a normal `Step` with a subagent display: description, type, model and reasoning, status, steps, duration, usage, and the report output id.
- Replay to the model sends the tool result (the report) only.

**Child transcript:**
- Its entries (steps and assistant text) are persisted next to the conversation's spooled outputs, as `agent-N.json`, with the same 30-day retention and deleted with the conversation.
- Reopening a conversation from history shows the subagent cards expanded from these files. A missing file shows the summary only.

**Compaction of the main log:** a subagent step is a tool result like any other, so tier 1 can clear it, keeping the head and tail lines and its output id.

## 7. Memory and safety
- Subagents read instructions and the memory index, and may `view` memory files. **Only the main agent writes memory**; it can save conclusions from a report.
- Reports are untrusted tool output, the same rule as command output. The main prompt says so.
- Command output inside a subagent is untrusted for the subagent too, and the same red-line rules apply.

## Milestones
| # | Scope | Main files |
|---|---|---|
| **M1** | **Core.** The `agent` tool schema and validation; the subagent runner; the restricted tool host; the explore/general policies; model/reasoning override; limits; compaction; partial reports; report spooling; usage. Tests run against a fake model and fake host. | `shellmate-ai` (`subagent.rs`, `agent.rs`, `conversation.rs`, `policy.rs`) |
| **M2** | **App integration.** Foreground and background execution; the agent task kind in the registry (notices, `read_output`, `stop_task`, `/tasks`); the concurrency setting; approvals with `source` and a pending list; stop and lifetime rules; persistence of child transcripts. | `shellmate/src/ai/{host,exec,tasks,panel,panel_state,agent_ui}.rs`, `shellmate-store` |
| **M3** | **UI.** The subagent card (live, collapsed and expanded, report, stop); stacked approvals; the settings row. Proto scenes in both themes: `ai-subagent-running`, `ai-subagent-done`, `ai-subagent-approval` (two pending: main + subagent), `ai-subagent-background` (two running in the background), `ai-tasks-subagents`, `set-ai-advanced` (with the new row). | `shellmate/src/ai/*`, `proto/*` |

## Test outline
- **Tool availability:** only in the main agent; hidden without a background target; hidden inside subagents.
- **Validation:** model / reasoning per provider; concurrency limit error.
- **explore:** read-only runs; write and red-line are refused without asking, in every mode.
- **general:** follows `decide(mode, class)`; approvals carry `source`; a denial returns to the subagent.
- **Report:** the final message is returned and spooled; partial on the step limit, failure or stop; the usage footer.
- **Override:** the request uses the override model and reasoning; the window comes from that model's settings.
- **Background:** returns immediately; completion notice; `read_output` while running and after; `stop_task`; `/tasks`; `/clear` stops it.
- **Isolation:** the subagent's request body contains no main-conversation messages; the main context does not count the subagent's tokens.
- **Persistence:** the child transcript is saved and reloaded; a missing file degrades to the summary; deleting the conversation deletes it.
- **UI fixtures:** pending approvals keep FIFO order and stay independent; stopping a subagent removes its card.

## Out of scope
- Cross-host subagents.
- Custom subagent definitions (with skills).
- Nesting.
- `send_message` / resume of a finished subagent.
- Agent teams.
- Enabling provider parallel tool calls.

## Delivery and verification (2026-11-25)

M1, M2 and M3 are implemented. The final UI review covered running, completed,
expanded, failed, stopped, step-limited and background subagents, multiple
approvals, the task list and the concurrency setting in both themes.

Review fixes:
- Reopening an interrupted subagent now shows **已停止** immediately, including
  read-only history before a task registry exists. Live tasks keep their running
  status, and the stored conversation is unchanged.
- Collapsed report previews join their first three non-empty source lines into
  one paragraph so the renderer's three-line limit also applies after wrapping.
  Expanded Markdown and copying retain the complete report.

Validation:
- Workspace/all-target Clippy with `-D warnings` and the app build pass offline.
- Tests: app **751**, AI **121**, store **103**, core **79** passed (**1,054**
  total; five existing tests requiring external services or OS integrations remain
  ignored).
- Eight existing scenes in both themes match the M2 screenshots pixel for pixel:
  `ai-chat`, `ai-agent`, `ai-agent-run`, `ai-tasks`, `ai-tasks-subagents`,
  `ai-context-popover`, `ai-mode-popover`, and `set-ai-advanced`.
- Native fixture interaction verified expansion and copying the complete Markdown
  report in both themes. Clipboard contents were restored after the checks.
- GUI fixtures use temporary data/log/cache directories and an in-memory database.
  No live provider or SSH session was used; those paths are covered by mock tests.
