# P12 AI agent memory: instructions and auto memory (Claude Code style)

## Goal
Every Shellmate AI conversation starts from nothing today:
- The system prompt holds only the protocol, session label, terminal state and cwd.
- 「清除 AI 记忆」 only clears the current drawer conversation (`workspace/ai.rs:143`).
- Past conversations are stored per host (`ai_conversations.host_id`) but never read back.

P12 adds two layers, like Claude Code (CC), with Codex as a second reference:
1. **Instructions you write:** the equivalent of CLAUDE.md / AGENTS.md.
2. **Auto memory the AI writes:** the equivalent of CC's auto memory.

In an SSH client, CC's "per project" maps to **per host**.

P12 depends on P11 (`docs/plans/p11-agent-long-tasks.md`): the conversation log and the system-prompt builder with compaction re-injection, the tool framework, the settings UI and `/` commands.

## User decisions (2026-11-24)
| Topic | Decision |
|---|---|
| How auto memory is written | **CC style:** the main model writes memories during the conversation, through a tool. Codex's background two-phase extraction is not used. |
| Default | **On**, like CC. |
| Scopes | **Global + host.** The local terminal counts as one host (`local`). Temporary and unsaved connections use global only. No group scope. |
| Storage | **Files**, in CC's format. |
| Secrets | **No automatic redaction.** A prompt rule says never to save passwords, tokens or private keys to memory. |
| Remote project instructions | **Yes:** load `AGENTS.md` / `CLAUDE.md` from the remote cwd and its parents (§3). |
| Phase | A separate **P12**, after P11. |

## References (verified 2026-11-24)
**CC:**
- **Instructions:** CLAUDE.md files at managed, user, project and local scope are concatenated, broadest first. Each is loaded whole, up to 4 MiB per file, and re-read from disk after `/compact`. AGENTS.md is supported.
- **Auto memory:**
  - It is on by default and lives in `~/.claude/projects/<project>/memory/`.
  - `MEMORY.md` is an index, one line per memory, plus one topic file per memory. The frontmatter carries `type`: user / feedback / project / reference, and a `modified` timestamp set by CC.
  - The first 200 lines or 25 KB of `MEMORY.md` load at session start; topic files are read on demand.
  - Writing past the limit warns the model to shorten the index.
  - Memory skips what can be derived from the code and what CLAUDE.md already says.
  - `/memory` views, edits and toggles it. Memory files are exempt from the 30-day sweep.

**Codex:**
- AGENTS.md is limited by `project_doc_max_bytes`.
- Memories are off by default and generated in the background: a chat must be idle for 6 h and younger than 30 days; a phase-1 extraction model feeds a phase-2 consolidation agent.
- Memories unused for 30 days are dropped.
- `/memories` toggles use and generation per chat.
- Codex calls memories "a recall layer; required rules belong in AGENTS.md".

## 1. Layout on disk
```
~/Library/Application Support/Shellmate/memory/
├── global/
│   ├── AGENTS.md          # 全局说明 — written by the user (settings), editable by the AI only when asked
│   ├── MEMORY.md          # global auto-memory index
│   └── <topic>.md         # one memory per file
└── hosts/<host-id>/       # HostId; the local terminal uses "local"
    ├── AGENTS.md          # 主机说明 — host editor 「AI 说明」
    ├── MEMORY.md
    └── <topic>.md
```

**Topic file format** (CC-compatible):

```markdown
---
name: deploy-layout
description: 应用在 /srv/app，docker compose v2 部署，重启用 docker compose up -d
type: project            # user | feedback | project | reference
modified: 2026-11-24T10:00:00+08:00   # set by Shellmate on every write
---
<body>
```

**Retention:**
- Deleting a host deletes `hosts/<id>/`.
- Memory is **not** part of the P11 30-day spool sweep.
- Memory is machine-local, as in CC, and not part of sync or export for now.

## 2. The `memory` tool
**Commands** (modelled on Anthropic's memory tool, implemented client-side, so every provider works):
- `view(path, view_range?)`: a directory listing or file contents;
- `create(path, file_text)`;
- `str_replace(path, old_str, new_str)`;
- `insert(path, insert_line, insert_text)`;
- `delete(path)`;
- `rename(old_path, new_path)`.

**Paths:**
- `/memories/global/...` and `/memories/host/...`; `host` resolves to the current session's host.
- Unsaved connections get no `/memories/host`.
- Paths are normalized and traversal is rejected; only `.md` files are allowed.
- A file may be at most 64 KB; a scope may hold at most 200 files.

**Execution:**
- It runs locally, with **no approval** and no permission-mode involvement.
- Shellmate sets `modified` in the frontmatter of every created or edited topic file.

**Index limits:**
- After a write to `MEMORY.md`, measure it against 200 lines / 25 KB.
- Near the limit, the result reminds the model to keep one line per entry, merge or drop stale entries, and move detail into topic files.
- Over the limit, the write succeeds, but the result tells the model to rewrite the index, because anything past the limit is not loaded (CC behaviour).

**UI:**
- A create, edit or delete shows a compact chip in the conversation: 「已保存 1 条记忆 · 查看」 / 「已更新记忆」 / 「已删除记忆」. 查看 opens the memory manager on that file.
- A view is shown as a subtle 「已读取记忆」 line.

## 3. Remote project instructions
**Which sessions:** those with a background channel (SSH or local).

**When:**
- At conversation start and after each compaction (re-read, as CC re-reads after `/compact`);
- when the terminal cwd differs from the last load at the next user message.

**How:** one read-only exec, which needs no approval.
- It walks from the cwd up to `/` and reads `AGENTS.md`, then `CLAUDE.md`, in each directory.
- Each file is capped at 64 KB, with a truncation note; the total is capped at 128 KB.
- Files are ordered from the root down to the cwd, so the closest file is read last (CC order).

**Trust:**
- The files come from the server and act as instructions, so the first time a host shows a given path, the drawer asks once: 「此主机的 /srv/app/AGENTS.md 将作为 AI 说明加载 · 信任 / 忽略」.
- The answer is remembered per host and path, under the store key `ai_remote_doc_trust:{host}:{path}` = `trust` | `ignore`.
- Ignored files are never sent. A changed file is still trusted, as with CC's workspace trust: trust is given to a location, not a hash.

**Placement:**
- At conversation start, trusted files go in the system prompt (§4).
- A file found after a cwd change is appended to the log as a context entry. This avoids mutating the system prompt mid-conversation, which would break the prompt cache, and matches how CC loads nested CLAUDE.md into message history.
- UI: 「已加载远端说明：/srv/app/AGENTS.md」.

## 4. Assembling the system prompt
The order is fixed: broadest first, closest last.

1. the P11 base agent prompt, including the memory rules (§5);
2. 全局说明: `global/AGENTS.md`, whole, up to 64 KB;
3. 主机说明: `hosts/<id>/AGENTS.md`, whole, up to 64 KB;
4. remote project instructions (trusted only), each labelled with its path;
5. 全局记忆索引: the first 200 lines / 25 KB of `global/MEMORY.md`;
6. 主机记忆索引: the first 200 lines / 25 KB of `hosts/<id>/MEMORY.md`, labelled with the host name.

**Snapshot rule:**
- The prompt is snapshotted when the conversation starts and rebuilt after each compaction.
- Memory writes during a conversation don't change it; they show in the next conversation or after a compaction. The model already knows what it wrote.

**When the toggle is off:** with 自动记忆 off, parts 5–6 and the `memory` tool are omitted. Instructions (2–4) still load: instructions are not memory.

## 5. Memory rules in the agent prompt (Chinese, modelled on CC's guidance)
- **Types:**
  - `user`: your role and preferences;
  - `feedback`: corrections you gave and approaches you confirmed, with the reason;
  - `project`: background for this host that commands cannot quickly reveal, such as deploy layout, conventions, ownership or caveats;
  - `reference`: where to find things outside the server, such as dashboards or tickets.
- **When to save:** when the user says 记住/以后都…; when the user corrects you or confirms a non-obvious approach; when you learn durable background that is costly to rediscover.
- **What not to save:**
  - anything one quick command reveals, or volatile current state: versions, running containers, disk usage;
  - anything the instructions already say;
  - temporary details of the current task;
  - **passwords, tokens, private keys or other credentials, ever**.
- **How to save:**
  - Check the index first and update an existing memory rather than duplicating it.
  - Write one topic file plus one index line: `- [标题](file.md) — 一句话`.
  - Use absolute dates.
  - Global vs host: preferences go in global, server facts in host.
- **Using memories:** memories can be stale. Before acting on server state taken from memory, verify it with a read-only command. If a memory is wrong, fix or delete it.
- **Instructions:** edit `AGENTS.md` only when the user explicitly asks to change the instructions.

## 6. UI and commands
- **设置 › AI › 记忆:**
  - the 自动记忆 toggle (default on);
  - 全局说明 (an editor for `global/AGENTS.md`);
  - 记忆管理 (browse by scope: global / each host; view, edit, delete);
  - 在 Finder 中打开;
  - 清除所有记忆, with an in-UI confirmation.
- **Host editor:** a 「AI 说明」 multiline field, saved to `hosts/<id>/AGENTS.md`.
- **`/memory`:** opens the memory manager focused on the current host. It shows the instructions (global, host, trusted remote files with their trust state), the memories, and the auto-memory toggle.
- **Rename:** the existing 「清除 AI 记忆」 button becomes 「清除对话」, so it doesn't sound like the new memory feature.
- **Disclosure dialog:** add 「AI 说明与记忆也会随请求发送」.

## Milestones
| # | Scope | Depends on |
|---|---|---|
| **M1** Core | Paths and layout; the `memory` tool (6 commands, limits, traversal guard, `modified` stamping, index-limit warnings); system-prompt assembly with snapshot and post-compaction rebuild; the memory rules; the toggle; host-deletion cleanup. Tests. | P11 complete |
| **M2** Remote instructions | The cwd → root walk exec; caps; ordering; the trust prompt and store keys; the cwd-change context entry; re-read after compaction. Tests, plus an e2e against the test sshd. | M1 |
| **M3** UI | The settings memory section; the host editor 「AI 说明」; the `/memory` manager; the chips; the rename; the disclosure text. Screenshots of the affected scenes only. | M1, M2 |

## Test outline
- **M1:**
  - Each tool command, path normalization and traversal rejection, size and file-count limits, the `.md`-only rule.
  - `modified` is set on write.
  - Index warnings near and over 200 lines / 25 KB.
  - The system prompt has the order 1–6 and truncation at the limits.
  - With the toggle off, there is no memory tool or index, but the instructions stay.
  - The snapshot is unchanged mid-conversation and rebuilt after compaction.
  - An unsaved connection has no host scope.
  - Deleting a host removes its directory.
- **M2:**
  - Walk order from root to cwd.
  - AGENTS.md before CLAUDE.md in one directory.
  - The caps.
  - An untrusted file is never sent; trust and ignore persist.
  - A cwd change appends a context entry, not a system-prompt change.
  - Telnet, mosh and serial skip the walk.
- **M3:**
  - Settings round trip.
  - The host editor field persists to the file.
  - `/memory` opens on the current host.
  - The chip appears for writes only.
