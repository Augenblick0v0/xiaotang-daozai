# 岛仔「下载」页 + 新任务上岛 — 实施计划

- 格式：agent-execution-plan/v2
- 文档状态：ready
- 目标：岛上新增「下载」页，列出 IDM / Free Download Manager / Ghost Downloader 的最近下载任务（文件名、大小、已下载与进度、创建/完成时间、速度），每行可「打开文件 / 打开所在文件夹 / 删除文件」；有新的下载任务出现时自动上岛，卡片持续显示进度与速度，完成后自动收起。
- 范围：只做本机 Windows 版的读取与呈现（不做「暂停/继续/新建下载」等下发控制）。数据源阅读器为只读，删除只作用于已下载完成的本地文件且走回收站。
- 工作目录：`D:\Daozai_fork`（分支 `feat/download-center`，基线 `ffa8a31` + 本计划前一条提交「AI 任务页：本轮计时…」）
- 目标环境：本机 Windows（`x86_64-pc-windows-msvc`），交付物 = `target/x86_64-pc-windows-msvc/release/daozai.exe`
- 操作边界：允许改 `src/`、`Cargo.toml`/`Cargo.lock`、`docs/`；不允许在用户机上留探针/临时文件；不允许删除用户文件（除用户点删除且有回收站保障）。

## 事实基线（实测，见 docs/research/）

| 来源 | 数据位置 | 能拿到 | 拿不到 |
|---|---|---|---|
| IDM | 注册表 `HKCU\Software\DownloadManager\<数字id>`（345 个任务） | 文件名、总大小、已下载（均为 8 字节 LE u64）、状态、添加时间、失败原因 | 最终保存目录（只有全局保存目录 + 文件名）、可信实时速度（`Speed` 是残留值） |
| FDM | `%LOCALAPPDATA%\Softdeluxe\Free Download Manager\db.sqlite`（`downloads` 表 430 行） | 文件名/总大小/已下载（`files` BLOB，自研 vmscl 大端格式，已解出）、创建/完成时间 | 速度（库里没有）、实时进度（只在 flush 时落盘） |
| Ghost Downloader | `%LOCALAPPDATA%\GhostDownloader\tasks.jsonl`（JSONL，19 行） | 名称、总大小、step 级 `progress`/`receivedBytes`/`speed`、状态五态、创建/完成时间 | 任务级真实字节（`ΣreceivedBytes` 不自洽，必须用 step `progress`） |

关键实测：IDM 全量枚举 3.9 ms / 全量读 24.3 ms；FDM 变更探测 2.3 ms、全量 blob 解析 27 ms；Ghost stat 0.05 ms、全解析 0.82 ms ⇒ 统一 1.5 s 轮询、变更才深读。
本机 IDM 本体缺失（只剩 `IDMan.exe.BAK`）⇒ `Status=1/2`（下载中/暂停）与实时 `Speed` **无实测样本**，按「推断」实现并写进代码注释。
FDM 库为 rollback journal（非 WAL），只读打开 + 变更探测即可，不需要复制整库。

## 设计要点（契约，实现方按此落地）

1. 新增模块 `src/download_scan.rs`（pub）：`Source` / `DlState` / `DownloadItem` / `Roots` / `ScanConfig` / 纯函数 `scan()` / `Watcher`（照抄 `task_scan.rs:229-292` 的单飞线程 + PollClock 模式，`POLL_INTERVAL=1.5s`）。
2. `DownloadItem.id = "<源>:<原id>"`（如 `idm:548`、`fdm:1599`、`ghost:tsk_…`），点击命中与去重都靠它；`live: bool` 表示「该值是否实时可信」（下载器进程在运行且源支持）。
3. 新增页面 `DOWNLOAD_PAGE = 10`（第 11 页），`DOWNLOAD_H = 232`（与 AI 任务页同高）；三张索引表、`dots`、`descriptions`、迁移分支一次性改齐（见 research 报告 §1.2/§1.3 的 19 处清单）。
4. 设置项（`Settings` 四处齐全：字段 / Default 单行 / load / save）：`download_sources:[bool;3]`、`download_keep_minutes:u32`、`download_island:bool`、`download_island_speed:bool`、`download_confirm_delete:bool`。
5. 轮询门控：**页面可见 或 「下载上岛」打开**时才起扫描线程（比 AI 任务页多一个条件，因为上岛要在页面收起时也工作）。
6. 上岛：`AlertSource::Download` + `Alert.download: Option<String>`（存 `DownloadItem.id`），进度刷新走 `same_event` 覆盖；卡片每帧从 `self.download` 读活数据；运行中 `duration` 视为无穷，完成/失败后停 6 s 自动收起。收起态复用 `island_badge_reserve` 显示「进度环 + 速度」，与 AI 任务角标二选一（下载优先）。
7. 动作：打开文件 = `Cmd::Open(path)`；打开文件夹 = `Cmd::Reveal(path)`；删除 = 新增 `Cmd::DownloadDelete(PathBuf)`，在 `main.rs` 用 `SHFileOperationW(FO_DELETE|FOF_ALLOWUNDO|FOF_NOCONFIRMATION)` 送回收站，库内只发命令、不直接删。
8. 首次扫描/启动时的历史任务不上岛：只对 `created_ms` 在「本次运行开始之后」且未入过岛的任务弹卡（基线快照在第一次扫描时建立）。

## 工作安排

### T01 — 分支与基线（已完成）
- 状态：done
- 依赖：无
- 改动：`git checkout -b feat/download-center`；把工作区原有未提交改动（AI 任务页计时/阶段/窗口聚焦）单独提交为 `b571570`；`.probe/` 进 `.gitignore`。
- 产物：分支 `feat/download-center`（已推送 `origin`）。
- 验收：`git log --oneline -1`、`git ls-remote origin feat/download-center` 有该分支。

### T02 — 后端：三源只读解析 + Watcher
- 状态：ready
- 依赖：无（与 T03 并行；只写 `src/download_scan.rs` 与 `Cargo.toml`/`Cargo.lock`）
- 改动：
  - `Cargo.toml` 加 `rusqlite = { version = "0.43", features = ["bundled"] }`（仅 Windows target；已验证本机 26 s 可编译），随后 `cargo update -p` 保持 `Cargo.lock` 可用 `--locked`。
  - `src/download_scan.rs`：`Source`(Idm/Fdm/Ghost) / `DlState`(Downloading/Paused/Queued/Done/Failed/Unknown) / `DownloadItem` / `Roots`(idm_dir, idm_save_dir, fdm_db, ghost_jsonl) / `ScanConfig{sources,keep_minutes,now_ms}` / `scan()` / `Watcher`。
  - IDM：`Win32_System_Registry` 读 `HKCU\Software\DownloadManager`，过滤 `^\d+$` 子键；`FileSize`/`Downloaded` 按 8 字节 LE u64；`Status=3` 完成、`0` 未完成/失败（带 `lastResult`）、`1` 进行中、`2` 暂停（推断，注释写明）；`st_time` 为创建时间；`Speed` 仅在 `IDMan.exe` 存活时采信。
  - FDM：`rusqlite` 只读打开 `db.sqlite`（`mode=ro`），查 `downloads` 的 `id,url,destinationPath,title,creationTime,finishedTime,files`；新增 `vmscl` 解码器（大端：[u32 fileCount] × [u32 entryCount] × [u32 keyLen][UTF-16BE key][u32 type][u8 pad][value]；type 2/3=int32、4=int64、10=string(0xFFFFFFFF=null)、9/28=容器），取 `details.path`、`size`、`Σsections[].done`；容器内未知 type 必须跳过而不是 panic。
  - Ghost：`tasks.jsonl` 按 `(size, mtime)` 变更检测，逐行 `serde_json` 解析（任一行失败整轮丢弃）；任务名 `name`、大小 `fileSize`、进度按 `steps[].progress`、速度 `steps[].speed`、状态字符串枚举（未知值透传为 `Unknown` 并保留原文）。
  - 每个源都要「文件/库不存在 → 返回空」，不报错不 panic；`keep_minutes` 过滤 + 按 `DlState` 排序（进行中 → 暂停 → 队列 → 失败 → 完成，同档新的在前）+ 截断 40 条。
- 产物：`src/download_scan.rs`（含 `#[cfg(test)]` 单测：vmscl 解码用真实 blob 十六进制样本、IDM u64 解码、Ghost JSONL 解析、排序与过滤）。
- 验收：`cargo test --lib download_scan --features record --target x86_64-pc-windows-msvc -- --test-threads=1` 全绿；另加 `#[ignore]` 的真机校验测试，跑 `cargo test --lib -- --ignored live_` 时用本机真实数据比对 research 报告的三个数字（IDM id=548 FileSize=47504048、FDM id=1599 size=25137773、Ghost 19 条任务）。

### T03 — 页面注册 + 设置项 + 测试断言
- 状态：ready
- 依赖：无（与 T02 并行；独占 `src/model.rs`、`src/app.rs`、`src/extras.rs`、`src/lib.rs`、`src/download_ui.rs` 骨架、`src/ui_tests.rs`、`src/settings_redesign_ui_tests.rs`）
- 改动：按 research 报告 §1.2 的 19 处清单一次改齐：`DOWNLOAD_PAGE=10`、`DOWNLOAD_H=232`、`PAGE_NAMES`/`PAGE_SETTINGS_TABS`(tab 16)/`dots:[Spring;11]`/`expanded_height`/`page_title`/`arrow_y`/`render_page`、`model.rs` 两个数组 + Default + `page_order` 的 `len==10→11` 迁移 + `visible_pages (6..=11)`、五个 `download_*` 设置字段四处齐全、`extras.rs` 的 `SETTINGS_GROUPS`/`descriptions`(11 项)/`settings_page_icon`/`settings_tab_module`/分页 `match tab 16`（调 `download_settings`）、`lib.rs` 挂载两个新模块、`ui_tests.rs` 与 `settings_redesign_ui_tests.rs` 里所有写死 10 项/旧字符串的断言同步改。
- 产物：能跑起来但不含真实数据的「下载」页（`download_ui.rs` 骨架 + 设置面板 `download_settings` 全部开关），`App.download: download_scan::Watcher` 字段与初始化。
- 验收：`cargo test --all-targets --features record -- --test-threads=1` 全绿（此时 T02 可能未合入，允许先用 `download_scan` 的空实现桩，但**不得为了让测试过而删既有断言**）；新增测试：第 11 页标题/设置分页映射、10→11 迁移、页面可见性开关往返。

### T04 — 下载页 UI + 三个按钮
- 状态：ready
- 依赖：T02、T03
- 改动：`src/download_ui.rs` 补齐 `page_download`（4 行 × 行高 42，行内：源图标 16px、文件名（中段省略）、副行「大小 · 状态 · 时间」、右侧进度百分比；进行中行画 2px 进度条）、`download_tick`（门控：页面可见 || 上岛开）、底部汇总「共 N 条 · 进行中 M · 总速度 X」；每行三个按钮（打开/文件夹/删除，16×16，hover 高亮）命中后发 `Cmd::Open` / `Cmd::Reveal` / `Cmd::DownloadDelete`，`download_confirm_delete` 打开时按钮先变「确认删?」二次点击才执行。
- 产物：可点击的下载页。
- 验收：`cargo test --all-targets --features record` 全绿 + 新增测试：注入 3 条假 DownloadItem（完成/进行中/失败各一）后断言页面出现文件名文本、点「打开」产生 `Cmd::Open`、点「文件夹」产生 `Cmd::Reveal`、点「删除」在二次确认后才产生 `Cmd::DownloadDelete`。

### T05 — 新任务上岛（卡片 + 角标）
- 状态：ready
- 依赖：T02、T03
- 改动：`extras.rs` 加 `AlertSource::Download`、`Alert.download: Option<String>`、`enqueue_alert` 的 `same_event` 按 id 判重、`tick_alerts` 的 `allowed` 门控（`settings.download_island && visible_pages[DOWNLOAD_PAGE]`）、`draw_notice` 新增下载分支（文件名 + 进度条 + 速度，进度条照抄音量条 `HUD_TRACK_X/W`）、`app.rs` 的 `notice_size` 新尺寸档、`compact_notices` 放行；`island_badge_reserve` 加下载角标（下载优先于 AI 任务）；新增任务检测与「首次基线不弹卡」在 `download_ui.rs::download_island_tick` 内实现。
- 产物：新任务自动上岛，进度实时刷新，完成 6 s 后收起；收起态药丸显示进度环 + 速度。
- 验收：`cargo test --all-targets --features record` 全绿 + 新增测试：注入一条新 DownloadItem → 一帧后 `notice` 为下载卡且宽度按新档；进度更新不新增队列条目（数量不变）；完成后 6 s 内清空。手工：真机开 Ghost Downloader 下一个大文件，观察卡片进度与速度跟得上。

### T06 — 真机验证 + 交付
- 状态：ready
- 依赖：T04、T05
- 改动：`编译并测试-Claude.cmd`（build + 全量测试）；真机点检（下载页在 IDM/FDM/Ghost 三条数据上显示正确、三个按钮可用、删除进回收站可还原、新任务上岛）；按 `AGENTS.md` 覆盖 `target/x86_64-pc-windows-msvc/release/daozai.exe` 并核对桌面快捷方式目标与哈希，若岛仔在运行则切换到新程序并核对进程路径。
- 产物：新 `daozai.exe` + 验证记录（`docs/plans/2026-11-21-download-center-verify.md`）。
- 验收：`target\claude-check\build.log` 里 `BUILD_EXIT=0`、`TEST_EXIT=0`；回收站里能找到被删文件；岛上卡片与页面数值与三个下载器自身界面一致（±2 s 内）。

## 已决议（用户 2026-11-21 21:29 确认）

- **Q1 页面形态：新开第 11 页「下载」**，与 AI 任务页并列、同高 232，可在设置里开关与排序。
- **Q2 上岛行为：进行中常驻 + 完成/失败后 6 s 收起**。下载期间岛上一直有卡（文件名 + 进度条 + 速度）；多个任务时卡片显示「最近活跃」那个并带「+N」计数；收起态药丸角标显示进度环 + 速度。
- **Q3 删除：回收站 + 点两次确认**（第一次点击变「确认删?」）；只有 `DlState::Done` 且文件确实存在的行可点。
- **Q4 实测口径：IDM 不做实测**（本机无 IDM 本体，只剩 `IDMan.exe.BAK`）——IDM 与 FDM 的「下载中/实时速度」只做「能读到就显示、读不到就标上次记录」，不追求实测校准；**Ghost 必须做真实下载实测**：由我在本机起一个本地 HTTP 源、放一个大文件，用 Ghost Downloader 真实下载，边下边看岛上卡片的进度/速度、页面数字与 Ghost 自身界面对齐（这是 T05/T06 的核心验收，不是可选项）。

## 风险

- R1 持久化迁移写漏 → 老用户页面配置被重置、`page_order` 重复指向同一页（research R1）。
- R2 三张索引表 + `dots` 不同步 → `descriptions[page]` 越界 panic（research R2）。
- R3 岛上宽度：展开面板 419、角标预留只有 96/24/64 三档，下载卡与 AI 角标同时占位会撑宽抖动（research R3）。
- R4 轮询成本：FDM 全量 blob 解析 27 ms，必须「变更才深读」，且门控不能退化成常驻全量扫描（research R4）。
- R5 回收站删除是全新代码，误写成 `remove_file` 就不可恢复（research R6）；删除必须二次确认 + 只对 `DlState::Done` 的行可用。
- R6 IDM 实时速度/Status=1/2、Ghost 运行中 speed 均无实测（research E）。呈现时用 `live` 标记区分「实时」与「上次落盘」，UI 上后者加「（上次记录）」提示。
