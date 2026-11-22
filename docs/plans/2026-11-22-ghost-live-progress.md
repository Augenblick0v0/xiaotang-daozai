# 岛仔「下载」页：Ghost 真实进度与速度 — 实施计划

- 文档状态：**草案 v1**（等 `ghost-src` / `ghost-live` 两份实测报告回来后定稿「设计契约」与「通道选择」）
- 目标读者：实现方（我）+ 复核方
- 关联：`docs/plans/2026-11-21-download-center.md`（下载页主体）、`docs/research/2026-11-22-ghost-api.md`、`docs/research/2026-11-22-ghost-runtime-probe.md`

## 0. 目标

Ghost Downloader 的下载任务，在岛仔的「下载」页与上岛卡片上要显示**真实进度百分比**与**实时速度**，替换现在只会在完成瞬间跳 100% 的不确定进度条。

成功的判据（可验收）：
1. 一个正在下载的 Ghost 任务，在岛仔上下载页的行内与上岛卡片里，百分比**单调增长**（允许短暂回退 <2%）、速度非 0 且与限速源的设定值同量级（±30%）；
2. 拿不到实时通道时（Ghost 没开、版本不支持、接口失败）**自动回退**到现在的行为（总量 + 不确定条 + 「上次记录」标记），不报错、不空窗；
3. 全量测试绿、release 出包、桌面「岛仔－当前测试版」指向新 exe 并在运行。

## 1. 范围

- **改**：`src/download_scan.rs`（Ghost 数据源）、`src/download_ui.rs`（进度/速度语义）、必要时 `src/extras.rs`（卡片副行）、`src/ui_tests.rs` + 单测
- **不改**：IDM / FDM 的现有解析；下载页的页面注册、设置项、删除链、上岛调度（都是已验过的部分）
- 不做：Ghost 之外的下载器实时速度（FDM 库内无速度字段，IDM 的 Speed 是残留值）

## 2. 事实基线（已实测）

| 事实 | 证据 |
| --- | --- |
| 本机 Ghost 版本 4.5.11.0，`D:\Ghost Downloader\Ghost-Downloader-3.exe` | 文件版本信息 |
| 任务数据 `%LOCALAPPDATA%\GhostDownloader\tasks.jsonl` | 21 条用户真实任务 |
| 下载进行中 `steps[].progress / receivedBytes / speed` 恒为 0，完成瞬间跳 100% | 2026-11-21 实测，逐 9 s 采样 30 s |
| 目标文件创建即被预分配到完整大小，无 `.part/.tmp` 可观察增长 | 同上 |
| 完成后的记录 `files=[]`、`baseName=""`、`steps[].filePath=""`，路径只能 `outputFolder + name` | 同上 |
| 进程监听 14370（WebSocket，扩展桥）与 16800（未知），`/` 等常见路径 404 | 2026-11-21 探测 |
| 启动下载：`Ghost-Downloader-3.exe <URL>`（`ghostdownloader` 关联） | 注册表实测 |

结论：**在拿到源码/运行时证据前，不能假设 tasks.jsonl 之外的通道可用**——这正是本轮调研要解决的事。

## 3. 设计契约（已定稿）

| 项 | 契约 |
| --- | --- |
| 数据源 | `tasks.jsonl` 只当目录（名字、大小、创建时间、输出目录）；进度与速度一律取 WebSocket `task_snapshot`（Ghost `app/services/browser_service.py` 的 `_sendSnapshots` → `toTaskSummary`） |
| 连接 | `tungstenite` 连 `ws://127.0.0.1:<Browser.Port ?? 14370>/`，发 `hello{protocolVersion:2, token:Browser.BrowserExtensionPairToken}`，等 `hello_ack`，再发 `subscribe_tasks` |
| 推送 | Ghost 只在 speedMeter 事件（下载中 1 Hz）推给已订阅会话，并按 session 的 `lastSnapshot` 去重；所以到点没帧就重发 `subscribe_tasks`（服务端把去重清掉并立即回全量），等价 1 Hz 轮询 |
| 超时 | 读超时 900 ms；连续 60 轮（约 1 分钟）没有快照就判定断线重连；`snapshot()` 超过 5 s 没新帧返回 `None`（宁可不显示，不拿旧数字装实时） |
| 数值 | 以 `progress`（0–100）为准：`done = total × progress / 100`；`progress ≥ 99.95` 或状态 completed 时按 total 满算；`progress` 还没动才回退 `receivedBytes`（Ghost 的 receivedBytes 会残：bili 恒 0） |
| 速度 | 直接用快照 `speed`（Σstep.speed，字节/秒），只有 >0 才显示 |
| 状态 | COMPLETED→Done、RUNNING/DOWNLOADING→Downloading、PAUSED→Paused、WAITING→Queued、FAILED/STOPPED→Failed；不认识的状态保留原文进 detail |
| 回退 | 通道不可用 ⇒ 用 jsonl 值并标「上次记录」；`done == 0` 时画不确定条，不画假 0% |
| 依赖 | 复用已有的 `tungstenite 0.28`（handshake）与 `serde_json`，零新增 |
## 4. 工作安排

### T01 — 调研：源码通道（ghost-src，已完成）
**产出**：`docs/research/2026-11-22-ghost-api.md`
**验收**：给出至少一条「可拿到进度+速度」的通道，含端口/路由/字段/示例；或给出「不存在」的代码级证据（文件:行号）。

### T02 — 调研：运行时实测（ghost-live，已完成）
**产出**：`docs/research/2026-11-22-ghost-runtime-probe.md`
**验收**：用真实下载证明某接口的数值随下载推进变化；附 ≥5 个时间点采样与清理记录。

### T03 — 定稿设计契约（已完成）
**依赖**：T01、T02
**产出**：本文件 §3 从「候选」改成「定稿」，写明选定通道与字段映射。
**验收**：通道选择有实测支撑；回退路径明确。

### T04 — 数据层：Ghost 实时通道接入（已完成）
**依赖**：T03
**产出**：`src/download_scan.rs` 里 Ghost 源加实时读取（独立超时、失败回退），`DownloadItem.done/total/speed` 用实时值覆盖快照值；`live=true` 仅在有实时数据时为真。
**验收**：单测覆盖「实时成功 / 实时失败回退 / 字段缺失」；不引入 >5 ms 的额外阻塞（缓存 + 后台线程）。

### T05 — UI：进度与速度真正生效（已完成）
**依赖**：T04
**产出**：行内进度条按真实百分比填充；`subtitle` 显示真实速度；上岛卡片副行与角标用真实 `rate`/`progress`；`done==0` 时才走不确定条。
**验收**：ui_tests 断言（真实百分比 → 填充宽度、速度文案、`badge_of` 数值）。

### T06 — 真机验证 + 出包交付（已完成）
**依赖**：T05
**产出**：限速源真实下载下，岛仔显示百分比增长 + 速度；全量测试绿；release 构建；快捷方式与运行进程切换；提交并 push。
**验收**：截图/采样记录；`cargo test` 0 failed；exe 哈希与快捷方式目标核对。

## 5. 风险

| 风险 | 缓解 |
| --- | --- |
| 4.5.11 没有对外 API，只有 main 有 | 按 T01 结论决定：只有 main 有 ⇒ 回退到「更频繁落盘探测」或明确不可用，并把结论写进用户可见说明 |
| API 需要 token/开关 | 读配置或注册表拿凭据；拿不到就回退 |
| WebSocket 协议复杂 | 先用 HTTP；WS 需引入依赖时先评估体积与可靠性 |
| 实时通道阻塞 UI | 独立线程 + 800 ms 超时 + 缓存上一帧值 |
| 版本升级后接口变化 | 字段缺失一律回退，不 panic |

## 6. 已决议

- 用户 2026-11-22 02:09：先看开源代码，能拿到就加进去；「现在的进度条没有实际作用，也没有下载速度显示」，要一并修掉；完成后 push GitHub 并切换到新程序；要求组建团队、先做计划后执行。
## 7. 验证记录（真机）

- 2026-11-22 03:2x，本机限速源 `http://127.0.0.1:18081/`（`.probe\slow_http.py`，1 MB/s/连接，100 MB 文件，2 连接）。
- `ghost_live_probe.py`（独立 Python 客户端）连续采样：`0 → 8.00% → 16.00% → … → 92.44% → 100%`，速度 4–12 MB/s，完成帧 `progress=100` 而 `receivedBytes` 略小于 fileSize ⇒ 印证「完成按 total 满算」。
- Rust 侧 `live_scan_reports_a_running_ghost_download`（`#[ignore]`，自己拉起探针造下载）实测 `scan()` 输出：
  `scan_e2e.bin | 8.00% | 2.00 MB/s | 8.00 MB / 100.00 MB | live=true` → 一路涨到 `86.62% | 10.19 MB/s`，断言「有速度」「百分比不倒退」通过。
- 结论：Ghost 的真实百分比与速度都拿得到；进度条与速度文案现在有真实数据来源。

## 8. 交付记录

- 2026-11-22 04:05 release 构建成功（`编译测试版-Claude.cmd`，EXIT=0）：`target\x86_64-pc-windows-msvc\release\daozai.exe`，
  24,291,840 字节，sha256 `BBB71A4C7E818F138EB396BBE8745A5B0C2EDC6170033E27C03B84B848406598`。
- 桌面快捷方式「岛仔－当前测试版」目标 = 上述 exe，工作目录 `D:\Daozai_fork`；已用该快捷方式启动，进程 pid 45408、路径核对一致。
- 全量 lib 测试：1351 passed / 0 failed / 143 ignored（`cargo test --locked --lib --features record -- --test-threads=1`）。
- 备注：首次出包失败是因为旧实例（pid 43244）锁着 release exe（`failed to remove file …daozai.exe`）；先停进程再构建即通过。

## 9. 范围变更（2026-11-22 09:24）

- 用户决定：**IDM 不适配，只适配 Ghost Downloader**。
- 落地：`download_sources` 默认 `[false,false,true]`；`download_tick` 构造扫描配置时强制 `[false,false,<Ghost 开关>]`，
  IDM/FDM 即使旧配置里开着也不碰（不读注册表、不开 sqlite）；设置页只留 Ghost Downloader 一个来源开关；
  页面空态、设置说明、设置分组描述与页面描述都改成 Ghost 专用文案。