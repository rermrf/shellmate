# P2：SSH 连接（MVP）实施计划

目标：在 P0/P1（界面骨架 + 本机终端）的基础上，做出**可日常使用的 SSH 客户端**：
能保存主机、连上服务器、处理指纹与各种认证、断线自动重连、复制会话、跳板机。

文件 / 端口转发 / 监控面板分别属于 P3 / P4 / P5，本阶段对 SSH 会话保持禁用（提示"P3 实现"等）。
Mosh / Telnet / 串口属于 P7，主机编辑器里可见但不可选。

## 技术决定（已定，不要改动）

| 项 | 决定 |
|---|---|
| SSH 库 | `russh`（0.65.x）。只用纯 Rust 实现，不引入 libssh2 / openssl |
| 异步运行时 | 启动时创建一个后台 tokio 多线程运行时（2 个 worker），作为 gpui `Global` 保存；SSH 任务都跑在它上面，结果经 `async-channel` 回到 gpui（gpui 执行器里 await） |
| 新 crate | `crates/shellmate-ssh`：连接、认证、known_hosts、shell 通道、跳板、连接池。**不依赖 gpui**，以便快速单测 |
| 终端后端抽象 | 在 `shellmate-core` 新增 `io` 模块：`TerminalBackend` trait（write / resize / close）+ `OutputSink`（output / exited / disconnected 回调，`Send + Sync`）。`shellmate-term` 的 `Terminal` 改为持有 `Box<dyn TerminalBackend>`，本机 PTY 与 SSH 通道都实现它。**保持"在 IO 线程上解析进 grid、通过合并通道唤醒 UI"的现有设计** |
| 交互回调 | `shellmate-ssh` 通过 trait 向外要决定：`HostKeyVerifier`（新指纹 / 指纹变更）与 `Prompter`（密码、私钥口令、键盘交互多步提示）。均为 async，app 侧用 oneshot 通道桥接到 gpui 对话框 |
| 主机存储 | SQLite（`rusqlite`，`bundled` 特性），路径 `~/Library/Application Support/Shellmate/shellmate.db`，文件权限 0600。**数据库里不存任何密钥/密码** |
| 凭据 | `keyring` crate（macOS 钥匙串）。service = `Shellmate`，account 形如 `host:<host_id>:password`、`key:<identity_path>:passphrase` |
| known_hosts | 读写 `~/.ssh/known_hosts`（与 OpenSSH 共用）。新主机：用户确认后追加；指纹变更：必须用户明确确认，确认后**替换**旧条目（不是追加） |
| ssh_config 导入 | 自己写解析器（`shellmate-core`），支持 Host / HostName / User / Port / IdentityFile / ProxyJump；`Host *` 作为默认值；含通配符的其他 Host 条目跳过 |
| 保活 | russh `keepalive_interval = 30s`，`keepalive_max = 3`（与原型设置文案一致） |

## 里程碑（每个里程碑单独提交，完成后停下等待 review）

### M1：SSH 核心（无 UI）
- 新建 `shellmate-ssh` crate；`TerminalBackend` / `OutputSink` 抽象；`Terminal` 重构为后端无关（本机终端行为不变：退出后回车重启）。
- 连接：直连 + ProxyJump 链（跳板机 `channel_open_direct_tcpip` → `connect_stream`），每一跳都走同样的指纹校验和认证。
- 认证：ssh-agent、私钥文件（含加密私钥口令）、密码、键盘交互（多步，含 OTP）。认证顺序遵循主机配置的方式；失败给出清晰的中文错误。
- shell 通道：`request_pty("xterm-256color", cols, rows, …)` + `request_shell`，窗口大小变化 → `window_change`，`exit-status` → `exited(code)`，连接断开 → `disconnected(reason)`。
- 连接对象可开多个通道（为 M4 的"复制会话"做准备）。
- **测试**：用 russh 自带 server 在进程内起 127.0.0.1 随机端口的测试服务器，覆盖：密码 / 公钥 / 键盘交互认证成功与失败、新指纹与指纹变更回调、shell 回显数据、窗口大小变化、`exit-status`、ProxyJump（第一个测试服务器支持 direct-tcpip 转发到第二个）、服务器断开后报告 `disconnected`。

### M2：持久化与主机管理（无 UI 或仅数据层）
- SQLite 主机库：分组（名称、颜色色调、排序）、主机（名称、地址、端口、用户、认证方式、私钥路径、分组、标签、收藏、ProxyJump 引用、最近连接时间）。迁移用简单的 `user_version`。
- 钥匙串读写封装（可在测试中替换为内存实现）。
- `~/.ssh/config` 解析器 + 导入模型（标记"已存在"、`Host *` 作为默认值）。
- app 不再显示原型示例数据，改为读 SQLite（首次启动为空列表，侧栏显示空状态与"新建主机 / 从 ~/.ssh/config 导入"入口）。`sample.rs` 仅用于测试。
- **测试**：CRUD（内存数据库）、迁移、ssh_config 解析（多名 Host、注释、大小写、`=` 语法、`*` 默认值、Include 忽略）、导入去重。

### M3：连接界面
- 双击 / 回车 / "连接"按钮发起连接：立即开标签（黄点 + 主机名），终端里按原型逐行显示进度：
  `→ 连接 10.0.1.21:22 · 经由 bastion`、`✓ 主机指纹已校验 ED25519 · known_hosts`、`✓ 认证成功 · 私钥`。失败显示红色原因与"按回车重试"。
- 对话框（gpui-component Dialog）：
  - 新主机指纹：显示算法与 SHA256 指纹，"信任并连接 / 取消"。
  - 指纹变更：醒目警告，需勾选"我确认服务器已更换密钥"后才能继续。
  - 认证：密码 / 私钥口令（掩码输入）、验证码（6 位 OTP 输入）、键盘交互多步（显示第 n 项）；"保存到 macOS 钥匙串"勾选；错误时在对话框内提示，不关闭。
- 主机编辑器（新建 / 编辑 / 删除）：名称、地址、端口、用户、认证方式、私钥路径、分组、标签、ProxyJump（从已有主机中选，可多跳）、"密码存入钥匙串"。删除需二次确认。
- 导入对话框：列出 ~/.ssh/config 中的主机，可勾选，已存在的置灰标注。
- 状态展示：标签圆点（连接中黄 / 正常绿 / 失败红 / 断开灰）、侧栏主机头像状态点、标题副标题 `deploy@10.0.1.21:22 · SSH · 私钥 · 经由 bastion`、状态栏 `PTY 列×行 · xterm-256color · UTF-8 · 保活 30s`。
- SSH 会话的"文件 / 端口转发 / 监控"保持禁用，提示对应阶段。

### M4：韧性与多路复用
- 自动重连：意外断开后标签显示"重连 n/5"（黄），退避 1/2/4/8/16 秒；成功后在终端追加 `✓ 已自动重连` 并打开新 shell 通道（同一个 Terminal，保留回滚）；5 次失败后标红"连接失败"，回车或点击重试。远端 shell 正常 `exit` 则视为退出，不自动重连，回车重新连接。
- ⌘⇧T 复制当前会话：SSH 复用已有连接开新通道（无需重新认证），终端提示 `→ 复用到 … 的已有连接 · 新开 channel #n`；本机会话则新开 PTY。
- 连接池：按主机复用连接，最后一个通道关闭后断开。
- 关闭仍在连接中的标签会取消连接任务。

## M1 review 结论（2026-11-13，已通过）
- M2：`Secret` 移到 `shellmate-core`（`shellmate-ssh` 再导出），凭据层直接返回它。
- M2：主机库与钥匙串放新 crate `shellmate-store`（依赖 core；`rusqlite` + `keyring`），ssh_config 解析放 core（纯文本解析，可单测）。
- M3：很多服务器（PAM、跳板机）通过键盘交互要密码：钥匙串里有该主机密码时，第一轮 echo=false 的单个提示自动填入，失败后再弹框。
- M4：SSH 握手没有超时。加"连接看门狗"：不计等待用户输入（指纹/认证对话框）的时间，累计超过 20 秒即中止并提示。
- 已知限制（暂不处理）：ssh-agent 中密钥很多时可能触发服务器 MaxAuthTries；agent 证书与服务器主机证书暂不支持。

## M2 review 结论（2026-11-13，已通过，附两项修正）
- 修正 1：ssh_config 改为 OpenSSH 语义——按文件顺序遍历所有与别名匹配的 `Host` 块（含 `*.corp.io` 这类通配块与 `!` 否定），每个参数取第一次出现的值；`Host *` 不论位置都按这个规则处理。带通配符的块不作为导入候选，但其设置作用于匹配的别名。
- 修正 2：认证默认值——导入时没有 `IdentityFile` 的主机默认用 ssh-agent；`SshHost::from_host` 按 OpenSSH 默认顺序追加后备方式：配置的方式 → 另一种公钥来源（agent ↔ 默认私钥）→ 键盘交互 → 密码（服务器不提供的方式自动跳过）。
- M3：删除主机时同时删除钥匙串中的密码。
- 开发方式：子代理在 `../ssh-rs-agent`（分支 `p2-wip`）开发，共享 `ssh-rs/target`；review 通过后把 `feat/p2-ssh` 快进到对应提交，`ssh-rs` 始终可运行。

## M3 review 结论（2026-11-13，通过；4 项修正已在 e934238 完成并复核，2026-11-14）
- 已验证：OrbStack 中的 OpenSSH 10.3 上，GUI 首次连接 → 指纹对话框（指纹与 ssh-keyscan 一致）→ 回车信任 → 写入 known_hosts（OpenSSH 格式）→ 私钥认证 → 远端 shell，中文双向正常，远端 `stty size` 与状态栏 PTY 尺寸一致；6 个真实服务器 E2E 测试通过。
- 修正 1（缺陷）：勾选"保存到钥匙串"后，凭据只能在**它所属的认证方式真正成功**时写入（按跳与方式确认），不能因为整条连接最终成功就写入。例：密码输错 3 次后经键盘交互登录成功，不得保存那个错误密码。
- 修正 2：主题同步漏了 `button_primary*` 等按钮令牌，主按钮显示为黑/白而不是原型的蓝色（P0 遗留）。检查所有用到的 gpui-component 颜色令牌（按钮、复选框、开关、链接、列表选中）。
- 修正 3：对话框加遮罩（原型 `--scrim`：浅色 rgba(20,24,32,0.32)，深色 rgba(0,0,0,0.55)），位置垂直居中偏上，不要压住标题行。
- 修正 4：双击 / "连接" 已有会话的主机时切换到已有标签（与原型一致），复制会话只走 ⌘⇧T（M4）。
- 失败提示保留"按回车重新连接"。
- 修正 1 的附带决定：多跳连接中途失败时，已认证成功的跳的凭据仍会保存（它们已被服务器确认正确）。
- 开发注意：未签名的 debug 二进制每次重新编译后，macOS 可能再次询问钥匙串访问权限，这是系统行为。

## M4 review 结论（2026-11-14，通过）——P2 完成
- 已验证：OrbStack 中的 OpenSSH 上，8 个真实服务器 E2E 测试全部通过（含两个通道共用一条连接、关闭最后一个通道即断开连接、服务器端杀掉会话后报告断开并能重新连接）；截图核对了"重连 n/5"黄色徽标、`✓ 已自动重连`、5 次失败后标红"连接失败"、⌘⇧T 复用连接（channel #2）以及两个标签共用一个重连循环。
- 看门狗：20 秒预算覆盖 TCP、握手、认证、每一跳和开 shell，等待用户（指纹/认证对话框）时暂停。
- 修了 M3 遗留的连接泄漏：本地关闭通道后输出泵仍持有连接，导致最后一个通道关闭后连接不断开。
- 重连按连接进行（`ConnGroup`），不按标签；已认证的凭据只存在内存中（连接组释放时清零），重连不再询问；一次性验证码从不记忆，指纹变更总是重新询问。
- 已知小问题（P3 开头顺手修）：断线那一行仍带"按回车重新连接"，紧接着又有"N 秒后自动重连 · 按回车立即重试"，略重复；即将自动重连时应去掉前一句的后缀。
- 未覆盖：GUI 中关闭仍在弹框的标签、回车/点击徽标重试只做了单元测试（不能用合成输入）；macOS 睡眠唤醒后要等保活（最长约 90 秒）才发现断线，后续可监听唤醒通知主动探测。

## 通用要求
- 界面文案用中文，措辞参考原型（`app_logic.js` 中的 `authSteps`、`openSession`、`healed` 等）。
- 不打日志输出密码、口令、私钥内容；内存中的密码用完即丢（可用 `zeroize`）。
- 每个里程碑：`cargo build`、`cargo test --workspace`、`cargo clippy --workspace` 零警告。
- 不用合成键盘 / 鼠标事件测试 GUI（会误发到其他应用）。可以启动 app 并用 `screencapture -l <窗口ID>` 只读截图。
- 新增的 Lucide 图标必须加进 `crates/shellmate/src/assets.rs`（默认资源只有约 100 个图标），并补到测试列表。
- gpui-kit 在 Cargo 中被重命名为 `gpui`；gpui-kit 的 `Root` 会绑定 tab/shift-tab（终端里已在 `Terminal` 上下文覆盖），新增全局快捷键时注意上下文优先级。
