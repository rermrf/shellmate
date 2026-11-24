# P11 AI agent: Claude Code–style long tasks

## Goal
The P8 agent stops on small hard-coded limits:
- 15 steps;
- 64K chars of tool output per question;
- 48K chars of history;
- `max_tokens` 2048;
- a 120 s command timeout.

It also loses context between questions. So multi-step jobs such as building and deploying a service don't finish.

P11 makes the agent behave like Claude Code (CC), using Codex as a second reference:
- unlimited turns;
- real context management in tokens, with compaction;
- CC's tool-output, command-timeout and output-token behaviour, including background commands;
- `/` commands.

## User decisions (2026-11-24)
| Topic | Decision |
|---|---|
| Turns | Unlimited by default, like CC. A setting changes it. |
| Context window | A setting. Auto-detect from rig's catalog. An unknown model counts as **128K**, and the UI suggests setting the real value. |
| Auto-compact window | A separate setting, like CC `/autocompact`. Trigger and strategy are in §2. |
| Compaction summary | A generic template modelled on CC and Codex. Not ops-specific. |
| Tool output | Like CC (§3.2). |
| Command timeout | Like CC: default 2 min, max 10 min. A timeout moves the command to the background; it is never killed (§3.3). |
| Output tokens | Like CC: per model, 32 000 for unknown models, cap 128 000. |
| Redaction | **Removed** from the AI path; CC and Codex don't redact. |
| SSH background tasks | Channel-based. They end when the connection drops and leave nothing on the server. |
| Output file retention | Like CC: 30 days by age, deleted along with the conversation, **no size cap**. |
| Settings UI | Main area: 上下文窗口, 自动压缩窗口, 单轮步数上限. 高级: 单次输出 token, 工具输出长度, 命令超时. |
| Persistence | The full cross-question message log and the compaction summaries are persisted. |
| `/` commands | Yes. Skills plug in later, compatible with CC / Codex `SKILL.md`. |
| Not now | Post-turn compaction (Codex), idle compaction (CC), steering mid-run, file tools, subagents. |

## Findings that drive the design (as of 528a69b)
- **Steps:** 15, clamped (`agent.rs:229/:487`).
- **Output per question:** 64K chars, kept across 「继续」 (`agent.rs:478`, `Resume.output_chars`).
- **Model text:** a 256 KB cap (`agent.rs:578`).
- **Same-command refusal:** `last_run`.
- **History:** 48K **chars** (`conversation.rs:12`). Above it, `prepare` `break`s (`:256`), dropping the newest heavy question **and every older one**. After a long deploy, the next question starts from nothing.
- **Between questions:** only flattened text survives, 8 KB per step. Tool calls and results are not replayed as real messages.
- **Output capture keeps the head only:**
  - SSH exec stops collecting at 256 KB **and closes the channel, killing the command** (`shellmate-ssh/src/exec.rs:120/:146`);
  - local and terminal capture also keep only the first 256 KB (`ai/exec.rs:124`, `shellmate-term/src/agent.rs:47`).
- **A timed-out SSH command loses all its output** (`ai/exec.rs:69`, `shape([], [])`).
- **Model call:** `max_tokens` 2048. A truncated tool call is a fatal `InvalidResponse` (`agent.rs:619`). The total-duration timeout of 195–390 s (`config.rs:132`) includes generation, so bigger outputs would time out.
- **Errors:** any error discards the question's state (`fail()`). There are no retries.
- **Anthropic prompt caching is off.** rig supports `CacheRetention`; Shellmate never sets it.
- **Redaction corrupts real work.** The model sees `PASSWORD=[已去除:凭据]` from `cat .env` and can write the placeholder back into files.

## 1. Conversation log (the base of everything else)
Replace "turns + flattened answers" with an append-only **log** per conversation, in `shellmate-ai`, using our own serde types (not rig's):

```
Entry =
  User { text, context: ContextSnapshot }          // context = the terminal snapshot taken when sent
  Assistant { text }                                // visible text
  ToolCall { call_id, name, args_json }             // a real tool call, replayed as tool_use
  ToolResult { call_id, inline, output_id?, exit_code?, status, cleared: bool }
  CommandCard { .. }                                // propose_command (non-executing sessions)
  Compaction { summary, retained_user: Vec<String>, state: RetainedState, before_tokens, after_tokens, trigger }
  Notice { kind, text }                             // background task finished, retry, pause ...
```

- **Building a request:** take the system prompt, then the latest `Compaction` (if any) rendered as its prefix block (§2.4), then every entry after it, as real rig messages: `ToolCall`/`ToolResult` become tool_use/tool_result.
  - `ToolResult.cleared` renders the stub (§2.3).
  - Anthropic signed reasoning blocks stay in memory only. They are attached to the assistant message of the current run and are never stored.
- **The UI renders the whole log.** Entries before a compaction stay visible, and a divider marks the compaction point (§2.6). The model sees only the compacted view.
- **Persistence:** store migration **v10** adds `ai_log(conversation_id, position, kind, json, created_at)` and converts v9 rows. User turns become `User`. Answer items become `Assistant` / `CommandCard`. Old `Step` items become a `ToolCall` + `ToolResult` pair with `cleared = true` and the stored 8 KB as `inline`. The old tables are left untouched for rollback.
- **Continuity:** `Resume` and step-limit continuation become "continue the log". Nothing new is lost on errors, because each tool result is appended as it arrives.

## 2. Context management
### 2.1 Sizes
- **W, the context window.** In order:
  1. the setting (`ai_context_window:{provider}:{model}` = `auto` | tokens);
  2. rig's catalog: the protocol's vendor first, then an exact-id lookup across the builtin catalog (OpenAI-compatible endpoints often proxy other vendors), using the input limit if present, else `context_window`;
  3. **128K**. The settings hint says 「未识别该模型，按 128K 计算，可手动设置」.
- **A, the auto-compact window.** The setting `ai_autocompact_window:{provider}:{model}` = `auto` (= W) | tokens. The minimum is `min(100K, W)`, as in CC, and A is capped at W.
- **O, the output tokens for a request** (§4.1).
- **R, the reserve:** `min(O, A / 4) + 1K`.
- **T, the trigger:** `A − R`. This is CC's rule: 1M gives 967K; 200K gives 167K; 128K gives 95K. There is no separate percentage setting; to compact earlier, lower A.

### 2.2 Accounting and triggers
- **Accounting.** Usage is the last call's `input_tokens + output_tokens`, as reported by the provider, plus an estimate of the entries appended since. The estimate counts each ASCII byte as 0.3 tokens, each other char as 1, and 8 per message. With no usage, estimate everything.
- **Triggers** (all three are required):
  1. **Pre-request:** before every model call, including mid-run between tool steps, compact if usage ≥ T.
  2. **Reactive:** when the provider rejects the request as too long (`ContextTooLong` and similar 400s), compact and resend once.
  3. **Manual:** `/compact [focus]`.
- **Not in P11:** post-turn compaction and idle compaction.

### 2.3 Tier 1: clear old tool results (no model call)
This tier runs first on any trigger.
- Every `ToolResult` except the **last 5** gets `cleared = true`. It then renders as:

  ```
  [较早的输出已移出上下文] 命令：<cmd> · 退出码 <n> · 原长 <chars>
  <first 3 lines> … <last 3 lines>
  完整输出：read_output(id="<output_id>")
  ```

  Output is spooled to files (§3.2), so this is nearly lossless.
- Clear everything eligible at once, never step by step, so the prompt cache breaks once per trigger.
- If usage is now ≤ 50% of A, stop. Otherwise go to tier 2. Manual `/compact` always goes on to tier 2.

### 2.4 Tier 2: summary compaction (CC `/compact` / Codex local compaction)
**The request:**
- the current compacted view, with tier 1 applied;
- in-flight tool results longer than 4K chars are cut to their head and tail;
- the summarization prompt appended as the final user message;
- no tools;
- `max_tokens` = `min(O, 16K)`.

If the request itself is too long, drop the oldest entries one at a time and retry, as Codex does. If one entry alone is too long, fail with the existing `ContextTooLong`.

**The summarization prompt.** It is modelled on CC's prompt (sections and an analysis scratchpad) and Codex's (a handoff to the next model). It is written in Chinese, and the focus text from `/compact` is appended:

```
你正在执行上下文检查点压缩：为将接手这项任务的另一个模型写一份交接摘要，使它能无缝继续工作。
先在 <analysis> 中按时间顺序梳理整段对话：用户的每个请求、你采取的做法、关键决定、遇到的错误及如何解决、用户的纠正与反馈。
然后在 <summary> 中按以下部分输出：
1. 主要请求与意图：用户明确提出的所有要求，尽量保留原意
2. 关键概念与背景：涉及的技术、环境与事实
3. 涉及的文件与内容：查看、修改或新建过的文件，及必要的关键片段
4. 错误与修复：遇到的错误、处理方式，以及用户的纠正
5. 解决过程：已解决的问题与仍在进行的排查
6. 用户消息：按顺序列出用户的全部消息要点（不含工具结果）
7. 待办事项：明确尚未完成的工作
8. 当前工作：压缩前正在做的事，尽量具体
9. 下一步：与最近请求直接相关的下一步；如有必要引用原话，避免偏离用户意图
要求：简洁、结构化，保留继续工作所需的关键数据和引用。
{如有：额外要求：<focus>}
```

Only `<summary>` is kept; `<analysis>` is discarded. If `<summary>` is missing, use the whole text. If the text is empty, use 「（无可用摘要）」.

**The new view after compaction:**
1. the system prompt;
2. the prefix block, a user message, combining CC's continuation line and Codex's summary prefix:

   ```
   本会话延续自之前的对话，此前的上下文已压缩。以下是之前对话的摘要。你仍可看到工具的当前状态；请在已完成的工作基础上继续，避免重复。
   <summary>
   ```

3. the retained user messages: newest first, up to **20K tokens** (Codex's `COMPACT_USER_MESSAGE_MAX_TOKENS`). An oversized message is cut to its head and tail. Previous summaries are excluded;
4. **`RetainedState`**, kept by the program rather than by the summary:
   - commands the user denied, so the existing "never retry a denied command in another form" rule survives;
   - background tasks still running;
   - the output ids that `read_output` can still read;
   - the permission mode;
5. a fresh terminal context snapshot (the latest 40 lines);
6. then new entries.

A later compaction folds the previous summary in, so the view holds one summary at most, as in Codex.

### 2.5 Thrash guard
If three compactions happen with fewer than 2 model steps between them, stop auto-compaction for this run and show 「上下文刚压缩就又满了（可能是单条输出过大）。可以用 /clear 开始新对话，或调大上下文窗口」. This matches CC's thrashing error.

### 2.6 UI
- **Header:** 「上下文：host · 已用 37K / 1M」. The used figure turns amber at ≥ T − 10%.
- **While compacting:** 「正在压缩上下文…」 shows in place of 正在思考.
- **Divider:** 「已压缩上下文 · 812K → 41K · 查看摘要」, with an expandable summary. It is persisted, as the `Compaction` entry.
- **`/context`:** window W, auto-compact window A, trigger T, and used, split into system prompt, summary, messages and tool results.

## 3. Command execution (CC Bash semantics)
### 3.1 Tool surface
- **`run_command`:** `command`, `purpose`, `target` (terminal / background, as today), `timeout_secs` (default 120, max 600), and new **`run_in_background`** (bool).
- **New `read_output`:** `{ id, offset_line?, limit_lines? (≤ 2000), grep? }`. It reads a spooled output or background task output **on this Mac**; the model can't `cat` it from the server. Its result follows the same inline limits as §3.2.
- **New `stop_task`:** `{ id }`.
- Both new tools are read-only for the policy: auto-run, never an approval card.

### 3.2 Output: spool plus inline rules (CC)
**Spooling.** Every execution path streams stdout and stderr, interleaved in arrival order with stderr lines marked, to `<app data>/ai-outputs/<conversation>/<output_id>.log`:
- SSH exec channel, local process, and terminal capture;
- no more head-only 256 KB buffers;
- output volume **never ends a command**;
- the file is truncated past **64 MiB**, keeping the head and the tail; the command is killed past **5 GB**, as in CC.

`shellmate-ssh` `exec_with` gains a streaming or spooling variant that does not stop at `max_output`. The existing `exec_with` callers (the monitor) are unchanged.

**What the model receives:**

| Result | Inline content |
|---|---|
| **Success** | Up to **30 000** chars. Past that: `output_id`, total size, the first 2 000 chars, and 「完整输出已保存，用 read_output 查看或搜索」. |
| **Failure** | A head-and-tail excerpt of up to **10 000** chars, cut from the read-back window. |

- **Success** means exit 0. Exit 1 from `grep`, `rg`, `egrep`, `fgrep`, `find`, `diff`, `test`, `[`, `git diff` and `git grep` also counts as success (CC's list).
- **Failure** means any other non-zero exit, a timeout kill, or a signal.
- `output_id` is always returned, so the model can read the rest. This deviates slightly from CC, which gives failures no path.
- The **read-back window** is the 工具输出长度 setting, default 30 000, max 150 000.
- The inline success ceiling follows the same setting, capped at 128 000 (CC `bashOutputMaxChars`).
- ANSI, OSC and CR cleanup stays (`output.rs` `clean` without the redaction call).
- **Retention**, like CC's `cleanupPeriodDays`. There is **no size cap**.
  - A sweep runs at app start and once a day. It deletes spool files older than **30 days**. The conversation itself is kept.
  - Deleting a conversation deletes its spool directory.
  - With 保存对话 turned off, the spool is deleted when the conversation is closed or replaced.
  - `read_output` on a deleted output returns 「该输出已被清理，可重新执行命令查看」.

### 3.3 Timeouts and background tasks (CC)
**Foreground.**
- `timeout_secs` defaults to **120** and goes up to **600**; both are settings (高级).
- When the timeout is reached, the command is **moved to the background and not killed**. The exception is a command that starts with `sleep`, which is stopped.
- The tool result says: 「命令在 120 秒内未完成，已转到后台继续运行 · 任务 #3 · 输出 id …」.
- A `cd` inside a moved command does not carry over, and the result says so.

**`run_in_background: true`.** Returns at once with the task id and output id.

**Background tasks.**
- They have no time limit: CC's interactive sessions don't limit them.
- They end when:
  - the command exits;
  - `stop_task` or the UI stops them;
  - the session's connection drops (the channel closes);
  - the session tab closes;
  - the app quits.
- The registry lives per AI drawer and session binding.
- When a task finishes during a run, a `Notice` is appended before the next model call: 「后台任务 #3 已结束 · 退出码 0 · 用 read_output 查看输出」.
- When a task finishes while idle, the notice is added to the log and sent with the next question.

**Terminal target.**
- On timeout there is no Ctrl-C. The command stays in the user's terminal as a task and the terminal stays busy.
- Capture continues until the marker; its output id works with `read_output`.
- `stop_task` sends Ctrl-C.

**UI and policy.**
- The drawer shows a compact running-task strip: name, elapsed time, a stop button. `/tasks` lists all tasks.
- Every command still goes through the existing approval policy, background ones included.

## 4. Model calls
### 4.1 Output tokens (CC)
- **O by default:** for a known model, `min(catalog max_output_tokens, 32 000)`; for an unknown model, **32 000**.
- **Setting** (高级 · 单次输出 token): `auto` | 1 024…128 000, capped at the catalog maximum when it is known.
- **Anthropic budget-thinking path:** `max_tokens = max(O, budget + 4096)`, as today.
- **Fallback:** a 400 whose body mentions `max_tokens`, `max_completion_tokens` or `max_output_tokens` retries with 8 192, then 4 096. Scope it like `parallel_refusal`, and remember the working value for this client.
- **`finish_reason = length` on a tool call:** don't execute it. Append the assistant message without the cut call, plus a notice: 「上一次工具调用因达到单次输出上限（N）被截断，未执行。请拆成更小的步骤或缩短命令」. Then continue; this counts as an invalid call.

### 4.2 Timeouts, retries, pauses (CC)
- **Timeouts:** first response within **10 min**, stream idle **5 min**, reasoning activity resets idle (as today), and **no total-duration cap**.
- **Retries:** automatic, up to **10** attempts with exponential backoff (1, 2, 4, …, capped at 60 s), honouring `Retry-After`. They cover `RateLimited`, `Unavailable`, `Network`, and `Timeout` before the first token, but only if no content of this call has arrived yet.
  - `AgentEvent::Retrying { attempt, max, delay, error }` → 「{error}，{delay} 秒后重试（第 n/10 次）」.
- **After the retries,** or for a transient error mid-stream: **pause, don't fail.** The log is intact, and 「重试」 continues it. Non-transient errors stay terminal: auth, model not found, quota, config, cancel, disconnect.
- **Steps:** `Limits.max_steps: Option<usize>`, with **None (unlimited) by default**. The setting offers 不限 · 50 · 100 · 200 · 自定义. Reaching it pauses with 「已执行 N 步 · 继续」.
- **Removed:**
  - the 15-step clamp;
  - the 64K output cap and the `ToolOutputLimit` variant;
  - the 256 KB text cap;
  - the 48K char budget;
  - `last_run` (CC has no repeat refusal).
- **Kept:** three invalid calls in a row pause the run instead of failing it.

### 4.3 Prompt caching
- **Anthropic:** request `CacheRetention::Short`, which rig maps to top-level `cache_control: ephemeral`.
- **Fallback:** if a provider or proxy rejects it (rig `UnsupportedOption`, or a 400 mentioning `cache_control`), disable it for this client and retry once.
- **OpenAI** caches automatically.

## 5. Remove redaction from the AI path
- Delete `shellmate_ai::redact`, its tests and every call site:
  - `UserTurn::redacted`;
  - `clean()`'s final redact;
  - `AnswerItem::sanitize`'s redaction;
  - `recent_context`, keeping the 40-line tail logic;
  - `execution_prompt`'s cwd redaction;
  - the store's `redact` use.
- Remove the `redactions` UI badge 「已去除 N 处敏感内容」.
- Update the disclosure dialog (`ai/mod.rs:128`): 「将把最近 40 行终端显示文本、你的问题、本次对话的历史，以及 AI 执行的命令和输出原样发送到 {destination}。Shellmate 保存的登录凭据不会发送。」
- Update the store header comment.
- Unrelated `Debug` key masking (API key, mosh key) stays.

## 6. `/` commands
**Behaviour:**
- **Trigger:** input that starts with `/` opens a popup above the input, listing name, args hint and description, filtered by prefix. ↑↓ selects, Tab completes, Enter runs, Esc closes. It follows the current style plus Flyme OS 12.
- **Matching:** **only an exact command-name match is a command.** Anything else, such as 「/etc/nginx/nginx.conf 有什么问题」, is sent as a normal question.
- **While a run is in progress:** the input accepts commands. `/compact` queues to the next step boundary; `/tasks`, `/context` and `/help` run at once; `/clear` and `/mode` are refused with a hint (stop first). Plain questions are still blocked, because steering is out of scope.

**Registry:** `Command { name, args_hint, description, kind: Local | Agent | Prompt, while_busy }`. Sources are builtin now; user and skill later.

**Built-ins:**

| Command | Kind | Action |
|---|---|---|
| `/compact [重点]` | Agent | Manual compaction (§2.3–2.4) with an optional focus |
| `/clear` | Local | Start a new conversation; the old one stays in history |
| `/context` | Local | The usage breakdown (§2.6) |
| `/tasks` | Local | List and stop background tasks |
| `/mode [ask\|auto\|full]` | Local | Show or switch this host's permission mode |
| `/help` | Local | List commands |

**Skills (later, not P11):**
- `Prompt` commands load from `~/Library/Application Support/Shellmate/skills/<name>/SKILL.md`, the CC and Codex format (frontmatter `name`, `description`).
- The skill descriptions go in the system prompt, and the model gets a `use_skill` tool.
- P11 only makes sure the registry and parser support `Prompt` commands.

## 7. Settings (`ai/settings.rs`, store)
**Main area**, after 思考等级:
- **上下文窗口:** 自动 · 128K · 200K · 256K · 400K · 1M · 自定义 (K). The hint is 「自动：已识别为 200K」 or 「自动：未识别该模型，按 128K 计算」.
- **自动压缩窗口:** 自动（= 上下文窗口） · 自定义 (K, ≥ min(100K, W), ≤ W). The hint is 「将在约 167K 时自动压缩」, showing T.
- **单轮步数上限:** 不限 · 50 · 100 · 200 · 自定义.

**高级**, collapsible:
- 单次输出 token: 自动 · 自定义;
- 工具输出长度: 30 000 by default, 1 000–150 000;
- 命令默认超时 / 最长超时: 120 s and 600 s by default.

**Storage keys:**
- per provider+model: window, auto-compact window, output tokens;
- global: steps, output length, timeouts.

Invalid values fall back to the defaults.

## Milestones
| # | Scope | Depends on |
|---|---|---|
| **M1** Loop and model calls | §4 in full, §5. Keeps the current turn model; the app compiles with the existing UI. Paused / Retrying use the existing continue button and status line. | — |
| **M2** Context management | §1 and §2: the log model, the v10 migration, window resolution, the triggers, tier 1 and tier 2, the thrash guard, the header meter, the divider and summary view. `/compact` is wired as an internal action, ready for M4. | M1 |
| **M3** Command execution | §3: spooling (ssh, local, terminal), the inline rules, `read_output` / `stop_task`, the 120 / 600 s timeouts, auto-backgrounding, `run_in_background`, the task registry, notices, the task strip, retention. | M1 (and M2 for the log entries; M3 can start on the executor side in parallel) |
| **M4** `/` commands and settings | §6 and §7. | M2, M3 |

**Review per milestone:**
- diff review;
- `cargo clippy --workspace --all-targets -D warnings`;
- `cargo test` (shellmate-ai, store, the app `ai::` tests);
- the e2e and socket tests I run;
- screenshots only of the affected scenes.

## Test outline
- **M1:**
  - 120 sequential tool calls finish with `max_steps = None`;
  - `Some(50)` pauses and resumes;
  - 429 → retry → success;
  - 10 × 503 → pause → resume works;
  - a mid-stream error pauses with no auto-retry;
  - `max_tokens` 400 → 8192 → 4096;
  - a truncated tool call is not executed;
  - the cache fallback;
  - no redaction anywhere (a fixture with `PASSWORD=x` reaches the request body verbatim);
  - migration-free (M1 doesn't touch the schema).
- **M2:**
  - window resolution (setting, catalog by vendor, cross-vendor id, 128K fallback);
  - T for 128K / 200K / 1M and a custom A;
  - the pre-request trigger mid-run;
  - the reactive trigger on a too-long 400;
  - tier 1 clears all but the last 5, ids stay paired, and the stub has a `read_output` id;
  - tier 2 request shape, `<summary>` extraction, the prefix, 20K retained user messages, `RetainedState` (denied commands survive), folding a second compaction;
  - drop-oldest when the compaction request itself is too long;
  - the thrash guard;
  - v9 → v10 conversion and round trip;
  - reopening a conversation continues from the compacted view.
- **M3:**
  - 1 MB of output: the command completes, the spool has head and tail, and inline follows the success / failure rules (with the benign exit-1 list);
  - an SSH command is no longer killed at 256 KB;
  - a foreground timeout moves the command to the background and the task finishes with a notice;
  - `sleep` is stopped at its timeout;
  - `read_output` offset / limit / grep;
  - `stop_task` on ssh, local and terminal;
  - a disconnect ends the task;
  - the 30-day retention sweep and deletion together with the conversation.
- **M4:**
  - command parsing (exact match only; a path-like question is not a command);
  - commands while busy;
  - settings round trip and clamping;
  - the popup scene screenshot.
