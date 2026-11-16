# P5：监控 实施计划

目标：做出原型中的**监控**视图——概览、进程、服务、Docker 四个子页——数据来自远端 Linux 主机，经同一条 SSH 连接上的独立 exec 通道采集；可以对进程、服务、容器执行常用操作；容器端口一键建立 P4 的端口转发。

本机会话的"监控"保持禁用，提示"本机状态请使用「活动监视器」"（与原型一致）。

原型参考：`docs/Shellmate 原型.html` 中的 `isMon`、`msubs`、`alerts`、`procAct`、`svcAct`、`ctrAct`、`ctrFwd`、`mDkOff`、`topProcs`、`cores`。截图：监控概览 / 进程 / 服务 / Docker。

## 技术决定（已定，不要改动）

| 项 | 决定 |
|---|---|
| 新 crate | `crates/shellmate-monitor`：探测、采集脚本、解析、速率计算、快照模型、操作命令的生成与转义、生命周期。**不依赖 gpui**，运行在 SSH 的 tokio 运行时上，结果经 `async-channel` 回到 gpui |
| `shellmate-ssh` 新增 | `Connection::exec(cmd) -> 输出 + 退出码`（有输出上限与超时）；`Connection::exec_stream(cmd) -> 按行的流`（持有连接，丢弃即关通道）。都用 exec 通道，不分配 pty |
| 采集方式 | **不在服务器上安装任何东西、不写任何文件**。一条长驻 exec 通道运行 POSIX `sh` 循环脚本，读 `/proc`、`/sys` 并按帧输出（如 `@@stats` … `@@end`）：系统数据每 2 秒、进程数据每 3 秒。脚本只用 shell 内建与少量常见命令，每轮 fork 不超过 3 个；通道关闭后脚本在下一次写入时因 SIGPIPE 退出，不留残余进程。所有命令以 `LC_ALL=C` 运行 |
| 速率在客户端算 | 远端只输出原始计数（`/proc/stat`、`/proc/[pid]/stat` 的 utime/stime、`/proc/net/dev` 字节数等），CPU%、每核占用、网络 MB/s、进程 CPU% 都由两次采样之差在客户端计算。解析与计算是纯函数，用真实系统输出做的样本（fixture）测试 |
| 平台范围 | 只支持 Linux（procfs）。glibc + procps 与 busybox 都要能用。非 Linux 主机：概览显示系统名与"暂只支持 Linux 主机的监控" |
| 何时采集 | 只在该主机有标签正显示"监控"视图时采集；离开后 10 秒停止（避免来回切换时反复启停）。断线时显示最后一次数据并标"等待重连"，重连后自动继续 |
| 归属与泄漏 | 每条连接（`ConnGroup`）一个 `Monitor`，与 P3 的 `SftpLink`、P4 的 `Forwarder` 并列；只持有 `WeakConnection`；最后一个标签关闭时 `shutdown()`，连接必须随之关闭（要有测试） |
| 探测 | 连接后（首次进入监控时）探测一次：`/etc/os-release`、`uname -sr`、运行时长、`sudo -n true`（是否免密 sudo）、是否 root、`systemctl --version`（systemd 版本）、docker / podman 命令与权限、页大小与时钟频率、`getent passwd`（uid → 用户名） |
| 需要 root 的操作 | 当前用户是 root：直接执行。有免密 sudo：`sudo -n`。都没有：**不在应用里收 sudo 密码**，改为"在终端中执行"——新开一个标签（复用连接），把 `sudo <命令>` 打进去，由用户在终端里输入密码。按钮上注明 |
| 命令安全 | 所有来自服务器或用户的值（服务名、容器名、路径）一律按 shell 单引号规则转义；PID 只接受整数。命令生成是纯函数并有测试（含带空格、引号、`$`、`;` 的名字） |
| 危险操作确认 | KILL、停止服务、停止容器、删除容器、清理镜像都先确认，文案按原型（如"强制结束进程 N？SIGKILL 无法被进程捕获，未落盘的数据会丢失。"） |
| 操作结果 | 底部提示（toast）显示实际执行的命令与结果（如"kill -TERM 2207 · 已发送"）；失败显示远端的错误输出（截断） |
| Docker | 经 docker 命令（`docker … --format '{{json .}}'`）而非直连 socket：兼容 podman-docker、rootless、`DOCKER_HOST`，权限问题可用 sudo。事件用 `docker events` 长驻流实时更新；CPU / 内存用 `docker stats` 长驻流（只在 Docker 子页可见时运行）。原型副标题改为如实的"经 docker 命令 · 事件实时推送" |
| 容器端口 → 转发 | 点端口标签：已发布到宿主机的端口（如 `127.0.0.1:3000→3000`）建 `-L 127.0.0.1:3000 → 127.0.0.1:3000`；未发布的端口（如 `6379/tcp`）用容器在其网络中的 IP 建 `-L 127.0.0.1:6379 → 172.18.0.3:6379`；已有相同目标的规则则提示"已有转发 …"并标 ✓。规则走 P4 的保存与启动流程，本地端口被占时按 P4 的建议端口处理 |
| 不记录内容 | 日志里不出现进程命令行、服务日志、容器日志（可能含密码等），只记命令种类与错误 |

## 各子页（按原型）

- **头部（所有子页共用）**：系统名（`PRETTY_NAME`）· 内核 · 已运行 · 负载；标签："sudo 免密" / "root 登录" / "sudo 需密码"、"docker 组" / "无 Docker"、"systemd N" / "无 systemd"、"独立 exec 通道 · 2s"；右侧子页分段：概览（需要关注的数量）、进程（数量）、服务（失败数，红色）/（总数）、Docker（运行 / 总数，或"未安装"）。
- **概览**：
  - CPU 卡片（核数、负载、百分比、2 分钟走势线）；内存卡片（已用 / 总量、百分比、已用 + 缓存条、Swap）；磁盘卡片（各挂载点已用 / 总量条，排除 tmpfs / overlay / squashfs 等伪文件系统）；网络卡片（默认路由网卡，↓ / ↑ MB/s 与走势线）。
  - 需要关注：失败的服务（→ 服务子页并选中）、反复重启的容器（→ Docker 子页并展开日志）、使用率 ≥ 80% 的挂载点（"分析目录"→ 新标签运行 `du -xh --max-depth=1 <挂载点> | sort -rh | head`）、僵尸进程。没有时显示"一切正常"。不做原型里"约 11 天后写满"的预测（没有历史数据）。
  - 各核占用（最多 8 核，超出则合并显示）；有温度传感器时显示"CPU 温度"，没有就不显示。
  - 占用最高的 4 个进程，点击跳到进程子页并选中。
- **进程**：过滤（命令 / 用户 / PID）、排序 CPU / 内存 / PID、树形视图、"N / M 个进程 · 每 3 秒刷新"；表格列 PID、用户、CPU %（带条）、MEM %、RSS、状态（R 绿、Z 红）、命令。选中后底栏显示该进程，按钮：结束 TERM、重载 HUP、降低优先级（renice +5）、strace 跟踪（新标签运行 `strace -f -p PID`，需要时加 sudo）、强制结束 KILL（确认）。对别人的进程无权限时按上面的 root 规则处理。
- **服务（systemd）**：搜索、过滤 全部 / 运行中 / 失败 / 未运行；列表列 服务（名称 + 描述）、状态徽标、自启、内存。右侧详情：名称、描述、状态 + 起始时间、主进程 / 内存 / CPU 时间、启动 / 重启 / 重载 / 停止（确认）、开机自启开关、日志（`journalctl -u NAME -n 50 -f`，只在选中时跟随）。没有 systemd 时显示空状态说明。
- **Docker**：顶部卡片（Docker Engine 版本、"经 docker 命令 · 事件实时推送"、运行 / 总数、镜像数与大小、数据卷数、"清理"——确认后只清理悬空镜像 `docker image prune -f`，确认框里写明可回收大小）。按 compose 项目分组（项目名、`compose · <配置文件>`），其余归"其他 · docker run · 独立容器"。每个容器：状态圆点、名称 + 镜像、状态文字（"Up 11 小时" / "重启中 · 第 N 次 · OOMKilled" / "Exited (0) 2 周前"）、端口标签（一键转发）、CPU %、内存（有上限时"412 MB / 1 GB"）、终端（新标签 `docker exec -it NAME sh`，有 bash 时用 bash）、日志（行内展开，`docker logs -f --tail 100`；容器未运行时显示最近日志）、重启 / 启动、停止（确认）、删除（只对已退出的，确认）。没装 Docker、或没有权限访问时显示对应空状态（原型 `mDkOff` 的文案，权限问题另写说明）。

## 测试环境

现有的 `shellmate-e2e`（Alpine、procps、无 systemd、无 Docker）用于 M1 / M2 的真实服务器测试，也正好覆盖"无 systemd / 无 Docker"的空状态。

M3 需要一台有 **systemd 与 Docker** 的 Linux 主机。建议用 OrbStack 建一台 Ubuntu 机器（如 `shellmate-mon`），装 openssh-server 与 docker.io，只供测试使用——**需要用户同意后再建**。

## 里程碑（每个里程碑单独提交，完成后停下等待 review）

### M1：采集引擎（无 UI）
- `shellmate-ssh`：`exec` / `exec_stream`（输出上限、超时、退出码、stderr 分开；丢弃即关通道），进程内测试。
- `shellmate-monitor`：探测；采集脚本与帧格式；`/proc/stat`、`/proc/meminfo`、`/proc/loadavg`、`/proc/uptime`、`/proc/net/dev`、`df -P`（或 `/proc/mounts` + `statfs` 的等价做法）、温度、`/proc/[pid]/stat` 与命令行、uid → 用户名的解析；速率计算；快照模型；命令生成与转义；`Monitor` 生命周期（开始 / 暂停 / `connection_lost` / `connection_restored` / `shutdown`）。
- **测试**：解析与计算用真实输出样本（glibc/procps 与 busybox 各一套，含边界：进程在两次采样之间消失、计数器回绕、无温度、无 swap、很长的命令行、命令行含换行和非 UTF-8）；命令转义；生命周期与泄漏（`shutdown` 后连接关闭；停止后远端没有残留的采集进程）。真实服务器（`#[ignore]`，`shellmate-e2e`）：探测结果、连续 30 秒采样数值合理（与 `docker exec` 读到的 `/proc` 对照）、进程列表包含已知进程、停止后 5 秒内远端无残留、采集脚本在远端的 CPU 占用（报告数字，目标单核 < 1%）。

### M2：概览与进程（UI）+ 接线
- 接线：`ConnGroup` 持有 `Monitor`；视图可见性驱动开始 / 停止；断线 / 重连 / 关闭。启用"监控"标签（本机会话仍禁用）。
- 概览与进程两个子页按原型；服务 / Docker 子页在 M2 先显示探测结果（"无 systemd" / "未安装 Docker" 的空状态，或"将在后续版本提供"——以探测为准）。
- 进程操作：TERM / HUP / renice / strace / KILL，含权限不足时的 sudo 与"在终端中执行"。
- 走势线、条形图用 gpui 绘制，深浅色都对。
- **测试**：模型（不依赖 gpui）单测：头部标签、需要关注、排序过滤、树形、按钮可用性与文案；真实应用截图（浅色 / 深色）对照原型；真实服务器上：数值在动、对自己起的 `sleep` 进程发 TERM / KILL / renice 生效、无权限时走"在终端中执行"、断线重连后恢复、关闭最后一个标签后无泄漏。

### M3：服务与 Docker（UI）+ 需要关注
- 服务子页与 Docker 子页全部功能；概览的"需要关注"接上服务与容器；容器端口一键转发接到 P4。
- **测试**：命令生成与输出解析（`systemctl` 各版本的输出、`docker … --format '{{json .}}'`、compose 标签）用样本单测；真实主机（测试机）上：服务启停 / 重载 / 自启开关 / 日志跟随、一个启动失败的服务出现在"需要关注"、容器启停 / 重启 / 删除 / 日志 / 终端、一个反复 OOM 重启的容器出现在"需要关注"、端口一键转发后经 -L 能访问到容器、清理悬空镜像；截图对照原型。

## 通用要求
- 沿用 P2–P4 的通用要求（中文文案参考原型、不记录敏感信息、零警告、不用合成键鼠事件、只读截图、新增图标加入 `assets.rs` 与测试）。
- 只操作测试用的容器 / 机器，不动用户的其他容器（例如 `openssh-server-new`）和本机服务；测试监听只用 `127.0.0.1`。
- 测试里在远端起的进程、服务、容器，结束时清理干净。
- 每个里程碑：`cargo build`、`cargo test --workspace`、`cargo clippy --workspace --all-targets` 零警告；动到 ssh 代码时跑 SSH / SFTP / forward 的 e2e。

## M1 review 结论（2026-11-15，通过）

提交：`2b9506c`（`shellmate-ssh` 的 `exec` / `exec_stream`）、`fc3346d`（新 crate `shellmate-monitor`）。

- `exec`：stdout / stderr 分开、退出码、每路 1 MiB 上限（超出即关通道并标 `truncated`）、超时；`exec_stream`：按行、有损解码、单行 256 KiB 截断、慢读者时只缓存 256 行（之后靠 SSH 窗口让服务器等）、丢弃即关通道，持有连接（有泄漏测试）。
- 采集脚本：每轮最多 3 次 fork（每秒一次 `sleep`，进程帧加 `cut` + `ps`，磁盘帧加 `df`，二者错开）；帧标记带每次运行随机的令牌，服务器内容冒充不了；通道关闭后 1 秒内退出（SIGPIPE 被忽略时靠 `printf` 失败）。命令行用 `ps`（procps：`-eo pid=,uid=,args=`；busybox：`-o pid=,user=,args=`，启动时探测），只在进程号列表变化或每 11 帧时调用；`df` 只问本地文件系统（死掉的 NFS 不会卡住）。
- 速率在客户端算；命令生成是纯函数，名字单引号转义、拒绝以 `-` 开头的名字、PID 只收正整数；需要 root 时按"直接 / `sudo -n` / 在终端中执行"三种方式，应用不收 sudo 密码。
- `Monitor`：与 `Forwarder` 同样的结构（弱连接、任意线程可调用、`shutdown` 后不再发事件）；采集通道意外结束按 1 / 3 / 10 秒退避重启，用尽后 `Failed`。
- 真实服务器（`shellmate-e2e`，Alpine）：探测一次往返约 0.22 秒；30 秒采样与容器内 `/proc` 对照一致；烧满一核时总 CPU 12.6%（8 核中的一核 = 12.5%）；`sleep 1234` 以正确用户出现、经动作 API 的 TERM 生效；停止后约 2 秒内远端无残留。采集成本：13 个进程时约 0.13–0.20% 单核；320 个进程时约 0.8–1.0% 单核、约 1 MB/分钟。
- 复跑：clippy 零警告；`cargo test --workspace` 894 通过；e2e 监控 4/4、SSH 8/8、SFTP 17/17、forward 7/7。

并入 M2 一起修的小问题：本机跑探测脚本的测试会在开发机上执行 `sudo -n true`（改为桩命令目录，不碰本机 sudo）；转义时以 `=` 开头的词也加引号（zsh 的 `=cmd` 展开）；"分析目录"改用 `du -xh -d 1`（busybox 的 du 没有 `--max-depth`）；`/proc/[pid]/stat` 只截 28 个字段，进程名含多个空格时 RSS 等字段可能被截掉——补样本测试，必要时多截几个字段。

测试机：用户同意后已建 OrbStack 机器 `shellmate-mon`（Ubuntu 24.04、`/bin/sh` 为 dash、systemd 255、Docker 29.1.3），`tester@shellmate-mon.orb.local:22`，与 `shellmate-e2e` 同一把测试密钥，免密 sudo，在 docker 组。只供测试使用；机器会挂载 Mac 的文件（`/Users`、`/mnt/mac`），测试不得触碰这些路径。

## M2 review 结论（2026-11-16，通过）

提交：`d900929`（M1 遗留项：`=` 开头加引号、`du -xh -d 1`、`/proc/[pid]/stat` 取 40 个字段并补带空格进程名的样本、本机测试改用桩命令不再碰开发机的 sudo / docker；另外 df 跳过 virtiofs 等宿主机共享挂载、同一设备只算一次）、`d021a06`（监控页：概览与进程，接入工作区）。

- 接线：每条连接一个 `Monitor`，该连接任一标签显示监控时采集，离开 10 秒后停止，切换子页不重启；断线灰显并标"等待重连"，重连后新采集器启动；关闭最后一个标签后远端 sshd 会话、采集器与本机连接 6 秒内全部归零（两台主机）。
- 头部标签与子页：两台主机分别显示"sudo 需密码 / 无 Docker / 无 systemd"（Alpine）与"sudo 免密 / docker 组 / systemd 255"（Ubuntu）；服务与 Docker 子页在 Alpine 为对应空状态，在 Ubuntu 为"下一版本提供"占位。
- 概览、进程按原型；进程操作 TERM / HUP / 降低优先级 / strace / KILL（确认）：Ubuntu 上 root 进程经 `sudo -n` 结束；Alpine（sudo 需密码）新开标签把 `sudo kill -TERM <pid>` 敲好但不执行。
- 复跑：clippy 零警告；`cargo test --workspace` 929 通过；e2e 监控 7/7（两台主机）、SSH 8/8、SFTP 17/17、forward 7/7。采集成本约 320 个进程时 Alpine 0.96%、Ubuntu 0.60% 单核，约 1.2 MB/分钟。

并入 M3 一起改：8 核时概览右栏被截断（核数多于 4 时分两列，概览在窗口矮时可滚动）；刚开始采样时走势线只剩右下角一小段或一根尖刺（点不够时铺满宽度、网络图设最小量程）；进程列表里会出现 Shellmate 自己的采集脚本（隐藏它及其子进程）；"在终端中执行"用固定 700 ms 后粘贴，改为等 shell 首次输出后安静一会儿再粘贴。

## M3 review 结论（2026-11-16，通过）——P5 完成

提交：`55315b9`（`shellmate-forward::pick_local_port`：容器端口转发的本地端口被占时用 P4 的建议端口）、`8ba58d6`（服务页、Docker 页、需要关注接上服务与容器、M2 打磨项）。

- M2 打磨：多于 4 核时各核分两列（最多 16，之后合并），概览可纵向滚动；走势线至少 2 个点才画、铺满宽度，CPU 量程固定 0–100%，网络最小量程 10 KB/s；采集脚本在每帧带上自己的 `$$`，它及其子进程不进列表、计数和占用最高；"在终端中执行"改为等 shell 输出后安静 200 ms（最多 3 秒）再粘贴，不回车。
- 服务（systemd）：列表、搜索、四种过滤、详情（起始时间、主进程 / 内存 / CPU 时间、启动 / 重启 / 重载 / 停止（确认）、开机自启开关，static / masked 等状态正确显示）、选中时跟随 `journalctl`（必要时 `sudo -n`，无权限时说明原因）；解析覆盖 systemd 255 与旧版输出样式。失败的服务进入"需要关注"（带最后一条错误日志）与头部"N 失败"。
- Docker：按 compose 标签分组、端口标签、"重启中 · 第 N 次 · OOMKilled"（Docker 在按策略重启时会清掉 OOMKilled 标记，监控在重启循环期间保留它）、行内日志、容器终端（有 bash 用 bash）、重启 / 启动 / 停止 / 删除 / 清理悬空镜像（均有确认）；`docker events` / `docker stats` 只在页面可见时运行。
- 端口一键转发：已发布端口 → `127.0.0.1:<宿主端口>`，未发布 → 容器 IP；在测试机上经转发 curl nginx 得到 HTTP 200，经转发对 redis 发 PING 得到 +PONG；已有规则时显示 ✓ 并提示"已有转发 …"。
- 子代理自查发现并修复的泄漏：`docker events`、`journalctl -f`、`docker logs -f` 只在有事件时才写输出，客户端关闭通道后会在服务器上一直留着；现在这些命令经 `script::watched` 运行（`setsid` 成独立进程组，标准输入结束时只向该组发 TERM），关闭最后一个标签后 5 秒内服务器上无残留。我另外验证了命令自行结束时通道能立即关闭。
- 复跑：clippy 零警告；`cargo test --workspace` 997 通过；e2e SSH 8/8、SFTP 17/17、forward 7/7、监控 10/10（两台主机）。容器那条 e2e 依赖测试机上预先有 nginx:alpine / redis:alpine / alpine 镜像（测试机访问不了 Docker Hub，需经 `mirror.gcr.io` 拉取），我拉取后复跑通过，随后已删除。测试机已恢复干净（无测试单元、无容器、无镜像）。

遗留（不阻塞）：容器 e2e 应自行准备镜像（先试 Docker Hub，失败再试 `mirror.gcr.io`），跑完自行删除。
