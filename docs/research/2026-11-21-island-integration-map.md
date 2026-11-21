# 岛仔（Daozai）接入地图：新增「下载」页 + 新增「下载上岛」通知

- 调研日期：2026-11-21
- 调研方式：**只读**（未修改任何源文件），全部结论附 `文件:行号`
- 仓库版本：`daozai 0.6.66`（`Cargo.toml:1-3`）
- 证据标记：`[Observed]` = 本次实际读到的代码；`[Assumed]` = 需要落地时验证的推断（附验证命令）
- 参照样板：现有「AI 任务」页（`src/task_scan.rs` + `src/task_ui.rs` + `Settings.task_*`）
- **行号口径说明**：以下行号取自**当前工作区**（`git status` 显示 `src/app.rs`、`src/main.rs`、`src/model.rs`、`src/native.rs`、`src/task_scan.rs`、`src/task_ui.rs`、`src/ui_tests.rs` 已有未提交改动，早于本次调研），不是 HEAD。改动前请以工作区实际内容为准。

---

## 0. 结论速览

| 问题 | 结论 | 主证据 |
|---|---|---|
| 页面枚举在哪里 | **没有枚举**。页面就是 `usize` 索引 0..=9，靠常量 + `match` 分发 | `src/app.rs:26-39`、`src/app.rs:93-94`、`src/app.rs:3268-3282` |
| 页面数量固定 10 的地方 | `visible_pages:[bool;10]`、`page_order:[usize;10]`、`PAGE_NAMES:[&str;10]`、`PAGE_SETTINGS_TABS:[usize;10]`、`app.rs:768 dots:[Spring;10]`、`model.rs:386 (6..=10)`、`model.rs:364-379` 三个迁移分支 | 见 §1.2 完整清单 |
| 新增第 11 页要动几个文件 | **6 个源文件 + 2 个测试文件**：`app.rs`、`model.rs`、`extras.rs`、新 `download_*.rs`、`lib.rs`、`main.rs`；测试 `ui_tests.rs`、`settings_redesign_ui_tests.rs` | §1.3 |
| AI 任务页接入点 | 状态 = `App.task: task_scan::Watcher`（`app.rs:920`），每帧 `task_tick()`+`task_island_tick()`（`app.rs:1878-1879`），渲染 `page_task()`（`task_ui.rs:54`），点击产出 `Cmd::OpenTask`（`task_ui.rs:111` → `main.rs:764`） | §2 |
| 上岛通知现成路径 | 四条：Alert 单槽（334×44）/ HUD（260×38）/ 会议大卡（360×136）/ **收起态岛角标**（`ISLAND_BADGE_W=96`，`app.rs:3029-3060`）。"持续更新"只有岛角标是原生的 | §3 |
| 打开/定位/删除 | 打开文件 = `open_path()`（`main.rs:996-1008`）；定位 = `Cmd::Reveal` → `explorer /select,`（`main.rs:763`）；**删除到回收站没有任何现成实现** | §4 |
| 设置界面 | 改 `extras.rs`（`SETTINGS_GROUPS`、`descriptions`、分页 `match`、`settings_page_icon`、`settings_tab_module`）+ `model.rs`；`resident_settings.rs` / `settings_window.rs` **都不是**设置项 UI | §5 |
| 构建 | `编译测试版-Claude.cmd` 只编译；`编译并测试-Claude.cmd` 编译 + 全量测试（`--features record --test-threads=1`） | §6 |

---

## 1. 页面注册机制

### 1.1 页面 = 纯 `usize` 索引，无枚举

```rust
// src/app.rs:26-39  [Observed]
pub const MUSIC_PAGE: usize = 0;
pub const FOCUS_PAGE: usize = 1;
pub const WATER_PAGE: usize = 2;
pub const SHELF_PAGE: usize = 3;
pub const TODO_PAGE: usize = 4;
pub const MAIL_PAGE: usize = 5;
pub const QUOTA_PAGE: usize = 6;
pub const CALENDAR_PAGE: usize = 7;
pub const WEATHER_PAGE: usize = 8;
pub const TASK_PAGE: usize = 9;
```

三张按索引对齐的常量表（**新增页面必须三张同时加**）：

```rust
// src/app.rs:93  页签/标题名
pub(crate) const PAGE_NAMES: [&str; 10] = ["音乐","专注","健康","置物架","待办","邮箱","AI 额度","日程","天气","AI 任务"];
// src/app.rs:94  页面 → 设置窗口分页序号
pub(crate) const PAGE_SETTINGS_TABS: [usize; 10] = [1,2,3,5,4,7,11,8,12,15];
```

绘制分发（唯一的 switch）：

```rust
// src/app.rs:3268-3282  [Observed]
fn render_page(&mut self, page: usize, c: &Card, env: &Env, cmds: &mut Vec<Cmd>) {
    match page {
        MUSIC_PAGE => ...,
        FOCUS_PAGE|WATER_PAGE|SHELF_PAGE|TODO_PAGE|MAIL_PAGE|QUOTA_PAGE
        | WEATHER_PAGE => ...,
        TASK_PAGE => self.page_task(c, env, cmds),      // ← 样板：独立文件里的 impl App
        CALENDAR_PAGE => ...,
        _ => self.page_settings(c),                     // 兜底 = 岛内简易设置页
    }
}
```

页面标题、页脚页码豆、「切页箭头」都在同一套索引上：

| 位置 | 作用 | 证据 |
|---|---|---|
| `page_title()` | 页眉两段文字 + 强调色 | `src/app.rs:3205-3226`；`TASK_PAGE => ("AI","任务",theme::SOFT_GREEN)` 在 `:3219` |
| `nav()` | 左右箭头 / 页码豆 / 拖拽排序后跳页 | `src/app.rs:3228-3266`；可见页过滤 `:3229`；箭头纵向位置 `:3231`（`page==6||page==8||page==TASK_PAGE → 88.0`，**新页若用 232 高也必须加进这个判断**） |
| `expanded_height()` | 展开高度 | `src/app.rs:1859-1861`（`TASK_PAGE → TASK_H=232.0`，`app.rs:41`） |
| `settings` 齿轮按钮 | 标题右侧 28×28 `c.hit(...,"page-settings")` | `src/app.rs:3162`；点击写 `self.settings_tab = PAGE_SETTINGS_TABS[self.page]` → `:3166` |
| `module_on(page)` | 页面是否启用（`visible_pages` 唯一读取入口） | `src/app.rs:1605` |
| `set_page_visible()` | 开关页面 + "不能关最后一个" | `src/app.rs:1628-1637`（`:1629-1630` 最小一页保护，`:1634-1635` 自动跳第一个可见页） |
| `navigate_page()` | 箭头滚动，按 `PAGE_NAMES.len()` 取模跳过隐藏页 | `src/app.rs:1640-1643` |
| 初始页 | 启动时第一个可见页 | `src/app.rs:939` |
| `compact_notices()` 依赖页 0 | 岛歌词 | `src/app.rs:1740` |

### 1.2 硬编码 10 的完整清单（新增第 11 页必改）

`[Observed]` 逐条来自 grep：

| # | 文件:行 | 内容 | 改成 |
|---|---|---|---|
| 1 | `src/model.rs:219` | `pub visible_pages: [bool; 10]` | `[bool; 11]` |
| 2 | `src/model.rs:220` | `pub page_order: [usize; 10]` | `[usize; 11]` |
| 3 | `src/model.rs:304` | Default 单行字面量 `visible_pages: [true; 10], page_order: [0,1,2,3,4,5,6,7,8,9]` | `[true; 11]` / `[0..10]` 追加 `10` |
| 4 | `src/model.rs:364-379` | `page_order` 迁移三分支：`len==10`→copy；`len==9`→`[9]=9`；`len==8`→`[8]=8,[9]=9` | 加 `len==10 → [..10]=order; [10]=10`；老的 9/8 分支尾巴补 `10` |
| 5 | `src/model.rs:386` | `"visible_pages" if (6..=10).contains(&v.len())` | `(6..=11)`（否则 11 字符配置整条丢弃→回全 true） |
| 6 | `src/app.rs:93` | `PAGE_NAMES: [&str; 10]` | `[&str; 11]` + 新名字（如 `"下载"`） |
| 7 | `src/app.rs:94` | `PAGE_SETTINGS_TABS: [usize; 10]` | `[usize; 11]` + 新分页序号（下一个空闲 = `16`） |
| 8 | `src/app.rs:768` | `dots: [Spring; 10]`（页码豆弹簧） | `[Spring; 11]` |
| 9 | `src/app.rs:3231` | `arrow_y=if page==6||page==8||page==TASK_PAGE {88.0}` | 加新页（若展开高度 > PH） |
| 10 | `src/app.rs:1859-1861` | `expanded_height()` 分支 | 加 `else if self.page==DOWNLOAD_PAGE { DOWNLOAD_H }` |
| 11 | `src/app.rs:3268-3282` | `render_page` match | 加 `DOWNLOAD_PAGE => self.page_download(c,env,cmds)` |
| 12 | `src/app.rs:3205-3226` | `page_title` match | 加标题两段 + 配色 |
| 13 | `src/extras.rs:1441-1445` | `SETTINGS_GROUPS: [(&str,usize,usize,&str);14]` | 追加 `("下载",16,16,"")`，类型改 `;15` |
| 14 | `src/extras.rs:1604` | `descriptions = [...10 项...]`（**用 `descriptions[page]` 索引**，长度必须 ≥ 页面数） | 追加第 11 项，否则越界 panic |
| 15 | `src/extras.rs:1285-1300` | `settings_page_icon(page)` match | 加 `DOWNLOAD_PAGE => ("download", include_bytes!(...))` |
| 16 | `src/extras.rs:1304-1307` | `settings_tab_module(tab)` match | 加 `16 => Some(DOWNLOAD_PAGE)`（否则 `ui_tests.rs:9296-9303` 断言失败） |
| 17 | `src/extras.rs:1569` 起的分页 `match tab` | 新分页要一个 `16=>{ ... settings_feature_header(...,DOWNLOAD_PAGE,...) ... }` 分支 | 样板见 `:1575-1579`（tab 15 = AI 任务） |
| 18 | `src/extras.rs:2313` | `_ =>` 是「关于」页，**不要**把新分页落到 `_` | — |
| 19 | `src/lib.rs:65-66` | `pub mod task_scan; mod task_ui;` | 加 `pub mod download_scan; mod download_ui;`（注意：`task_ui` 是私有 `mod`） |

持久化兼容逻辑只有 `model.rs:364-389` 两处 `[Observed]`：

```rust
// src/model.rs:364-379  page_order 迁移
"page_order" => {
    if order.len()==10 && (0..10).all(|page|order.contains(&page)) { s.page_order.copy_from_slice(&order); }
    else if order.len()==9 && ... { s.page_order[..9].copy_from_slice(&order); s.page_order[9]=9; }  // 旧配置里没有 AI 任务页
    else if order.len()==8 && ... { s.page_order[..8].copy_from_slice(&order); s.page_order[8]=8; s.page_order[9]=9; } // 没有天气页
}
// src/model.rs:386-389  visible_pages 迁移：新位继承"曾经开过任意页"
"visible_pages" if (6..=10).contains(&v.len()) && v.bytes().all(|b| b==b'0' || b==b'1') => {
    for (enabled,value) in s.visible_pages.iter_mut().zip(v.bytes()) { *enabled = value==b'1'; }
    for enabled in &mut s.visible_pages[v.len()..] { *enabled = v.bytes().any(|b| b==b'1'); }
}
// src/model.rs:446 兜底：至少一页可见
if !s.visible_pages.contains(&true) { s.visible_pages[0]=true; }
```

写出侧 `model.rs:473-474` 是按数组长度自动拼串（`"1"/"0"` 逐位、`page_order` 逗号拼接），**不需要改**；但**字符串长度会从 10 变 11**，见 §6 的测试断言。

### 1.3 断言"10"的测试

| 测试 | 断言 | 影响 |
|---|---|---|
| `src/ui_tests.rs:716`（`v0649_settings_and_mail_accounts_survive_upgrade`） | 读 8 字符 `visible_pages=10111111` / 7 项 `page_order` | 迁移分支必须继续可用 |
| `src/ui_tests.rs:731-732` | `assert saved == "visible_pages=1011111111"`、`"page_order=5,2,0,1,3,4,6,7,8,9"` | **必破**：新增第 11 页后应为 `10111111111`（或按迁移策略），要同步改期望值 |
| `src/ui_tests.rs:3332`、`:3350` | `assert_eq!(visible_pages,[true;10])` | **必破** → `[true;11]` |
| `src/ui_tests.rs:3340` | `assert_eq!(settings.visible_pages,[false,true,true,false,true,true,true,true,true,true])` | **必破** → 补第 11 位 |
| `src/ui_tests.rs:3346-3348` | 6/7 字符迁移期望，含"旧版页面开关应保留，并开启新增的日程页、天气页与 AI 任务页" | 需扩成也开新页 |
| `src/ui_tests.rs:6140`、`:6186-6189` | `page_order=[5,2,0,1,3,4,6,7,8,9]` 写死 | **必破** → 11 项 |
| `src/ui_tests.rs:9296-9303` | 标题表 16 项 + `pages.sort()==(0..PAGE_NAMES.len())` "每个功能页面各有一个设置分页" | **必破**：标题表加 `"下载"`（17 项）+ `settings_tab_module(16)==Some(新页)` |
| `src/ui_tests.rs:9307-9320` | `PAGE_NAMES[TASK_PAGE]=="AI 任务"`、`PAGE_SETTINGS_TABS[TASK_PAGE]==15`、`expanded_height()==TASK_H` | 以 AI 任务页为模板复制一份新页测试 |
| `src/ui_tests.rs:2515-2517` | `page_order=[5,2,0,1,3,4,6,7,8,9]` + `for page in 1..PAGE_NAMES.len()` | 同上 |
| `src/settings_redesign_ui_tests.rs:43` | `visible_pages=[false;10]` | → `[false;11]` |
| `src/settings_redesign_ui_tests.rs:68` | `for page in 0..9`（懒加载关闭判定） | 确认是否要把新页纳入 |

---

## 2. 以「AI 任务」页为模板的逐条接入点

### 2.1 状态放在哪里

```rust
// src/app.rs:920   [Observed]
pub(crate) task: crate::task_scan::Watcher,      // 扫描器 + 后台线程句柄 + 手动注入项
```

`Watcher` 自己持有：`items`、`manual`、`clock: PollClock`、`rx: Option<Receiver<Vec<TaskItem>>>`、`roots`（`src/task_scan.rs:229-236`）。
新页面照抄：`pub(crate) download: crate::download_scan::Watcher`（`app.rs:1023` 那一行巨型初始化处也要加 `download: Default::default()`）。

### 2.2 轮询节奏：后台线程 + PollClock 限流，帧内只做 `try_recv`

```rust
// 每帧（app.rs:1878-1879）
self.task_tick();
self.task_island_tick();

// src/task_ui.rs:37-52  task_tick()
if !self.settings.visible_pages[TASK_PAGE] { return; }              // 页面关 → 完全不起线程
let watching = (self.open && self.page==TASK_PAGE)                  // 展开且正在看
            || (self.settings_open && self.settings_tab==15);       // 或设置页停在该分页
if !watching { return; }
self.task.tick(std::time::Instant::now(), crate::task_scan::ScanConfig{
    sources: self.settings.task_sources,
    keep_minutes: self.settings.task_keep_minutes,
    now_ms: now_ms(),
});

// src/task_scan.rs:260-292  Watcher::tick
// 1) try_recv 收上一轮结果 → demote_stale；2) 若已有线程在跑 或 PollClock 未到点 → return
if self.rx.is_some() || !self.clock.ready(POLL_INTERVAL) { return; }
// 3) std::thread::Builder::new().name("daozai-tasks").spawn(scan → tx.send)   —— 每次最多一个在飞
```
`POLL_INTERVAL = 2s`（`task_scan.rs:29`），扫描是**纯函数** `scan(&Roots, &ScanConfig)`（`task_scan.rs:183-215`），便于单测。
**这是性能关键**：绝不能在 `render` 里同步读盘；UI 侧只 `request_repaint_after(500ms)`（`task_ui.rs:67`）。

### 2.3 渲染入口

```rust
// src/task_ui.rs:54-130
pub(crate) fn page_task(&mut self, c:&Card, _env:&Env, cmds:&mut Vec<Cmd>)
```
- 常量：`AMBER=(255,166,77)`、`FAIL=(255,92,92)`、`ROW_TOP=42.0`、`ROW_H=42.0`、`ROWS=4`（`task_ui.rs:15-23`）
- 行命中：`c.hit(40.0, y-6.0, PW-52.0, ROW_H-4.0, ("task-row", task.id.as_str()))`（`:88`）——**id 用 `(&str,&str)` 元组，天然稳定**
- hover 底色：`c.fill(..., Color32::from_white_alpha((hover*10.0) as u8))`（`:91`）
- 点击：`cmds.push(Cmd::OpenTask(task.source, task.log.clone()))`（`:111`）
- 底部汇总：`c.text(15.0, TASK_H-16.0, &summary, ...)`（`:125`）

### 2.4 点击/右键/按钮交互

- 「点击整行进 `Cmd`」是主模式（`task_ui.rs:111`）→ 由外壳执行（`main.rs:764-777`）。
- 右键菜单样板：`self.shelf_menu_rect`、`self.task_dialog`（`app.rs:1723`、`:2205`、`:2221`）——菜单用 `egui::Area`/自绘矩形 + `notice_details` 同款"点外面关掉"逻辑。
- 岛上小按钮样板（最贴近下载卡片的「打开/删除」）：`fn notice_button(c:&Card,x:f32,y:f32,w:f32,text:&str,id:&str)->bool`（`extras.rs:937`），用法见 `extras.rs:483-487`、`:495`。
- 需要模态编辑器的样板：`draw_task_editor` / `task_editor_inputs`（`app.rs:2205`、`:2221`）。

### 2.5 图标与配色

```rust
// src/task_ui.rs:438-474  source_logo / source_texture
include_bytes!("../assets/ai-icons/deepseek.svg" | "codex.svg" | "claude.svg")
// 深色系直接 from_white_alpha(a)；品牌色用 from_rgba_unmultiplied(77,107,254,a)
crate::skin_paint::mark_monochrome(texture.id());   // 让玻璃皮肤能改色
```
设置侧图标另一套：`assets/settings-icons/system/*.svg`（`extras.rs:1539-1553`、`extras.rs:1288-1297`），**注意 `extras.rs:1284` 的注释**：纹理缓存只按地址区分不分尺寸，同一 `bytes://` URI 先小后大就会糊，所以卡片图标要独立 uri（`settings-header-*`）。
配色常量在 `src/theme.rs`（`theme::SOFT_GREEN`、`GRAY`、`WHITE`、`DIM`、`GREEN`）。

### 2.6 设置的读写（每个字段三处）

`Settings` 结构体字段（`model.rs:290-299`）→ `Default` 同一行字面量（`model.rs:304`）→ `load()` KV 分支（`model.rs:399-405`）→ `save()` 写回（`model.rs:491-495`）。当前任务页字段：

```rust
pub task_sources: [bool; 3],   // 三个来源开关
pub task_keep_minutes: u32,    // 5/15/30/60/240，非法值回落 30
pub task_hide_project: bool,
pub task_island_speed: bool,
pub task_island_progress: bool,
```

---

## 3. 「上岛」通知机制

### 3.1 数据模型

```rust
// src/extras.rs:9
pub(crate) enum AlertSource { Internal, Messenger(crate::app::Messenger), External(String) }
// src/extras.rs:33-44
pub(crate) struct Alert {
    pub title: String, pub source: AlertSource, pub body: String,
    pub page: usize, pub at: f64, pub duration: f64,
    pub bluetooth: Option<(crate::bt::DeviceKind,bool)>,
    pub meeting: Option<(crate::mail::Meeting,String)>,
    pub unread: Option<crate::app::Unread>,
}
```
`Alert` 是**快照**：`at` + `duration` 决定寿命，没有"更新内容"的通道 → 想持续更新只能靠"同事件替换"或每帧重算。

### 3.2 抢占与排队（唯一入口）

```rust
// src/extras.rs:286-312  enqueue_alert(&mut self, a:Alert, system:bool)
// · Internal 先写 history（truncate 50）                                          :288-289
// · fullscreen_hidden 时健康提醒直接丢弃                                          :290
// · settings.notifications 打开才入队：self.alerts.push_back(a); while len>8 pop_front  :307-310
// · same_event 判重（:298-301）→ 命中直接把 self.notice=Some(a) 覆盖并 return   :303-306
//   ← 这就是"刷新同一条"的现成机制：先 retain 掉队列里的旧同事件，再覆盖当前槽
```

### 3.3 单槽状态机 + 自动收起

```rust
// src/extras.rs:440-475  tick_alerts()
self.alerts.retain(&allowed);                                   // 按 source/设置过滤
if !settings.notification_preview { External 的 body 强制脱敏 }   // :457-461
if self.notice.as_ref().is_some_and(|a| self.t - a.at > a.duration) { self.notice = None; }  // :462-468
if self.notice.is_none() { if let Some(mut a)=self.alerts.pop_front() { a.at=self.t; self.notice=Some(a); } } // :469-474
```
**自动收起规则 `[Observed]`：纯时间——`self.t - a.at > a.duration` 就清空；没有任何鼠标悬停暂停/续期逻辑。**
悬停唯一的副作用是「把岛展开」（`app.rs:1981` 的 `on_notice` 命中 `notice_rect`/`notice_details_rect` → `app.rs:2048` `want=true` 维持展开），展开后 `notice_details` 可点开详情（`app.rs:2160`、`:2181-2191`）。

现有各来源的 `duration`：

| 来源 | 构造点 | duration |
|---|---|---|
| 健康提醒 | `extras.rs:249-259` | 瞬态 `2.5` / 常驻 `8.0` |
| 会议提醒 | `extras.rs:262-268` | `12.0` |
| 蓝牙 | `extras.rs:269-285` | `2.5` |
| 系统/第三方通知 | `extras.rs:413`、`:437` | `4.5`（并设 `notification_bell_until=self.t+4.5`） |

### 3.4 尺寸（谁占多宽）

```rust
// src/app.rs:1815-1818  notice_size()
meeting.is_some()      → (360.0, 136.0)
Internal+蓝牙设备       → (HUD_W=260.0, 48.0)
is_hud()               → (HUD_W=260.0, 38.0)      // 音量/大写锁定/输入法/网络/电源
其它                   → (334.0, 44.0)            // QQ 通知、第三方通知、普通 Internal
```
展开面板宽度 `PW=419.0`（`app.rs:24`）；收起药丸 `BAR_W=163.0`、`BAR_H=30.0`（`app.rs:54` 一带）。
`base_size()`（`app.rs:1825-1842`）决定窗口尺寸：未紧凑、未展开、有 notice → `notice 宽 + pet_space`；否则 `(resident.width + island_badge_reserve()).min(WIN_W-32)`，高 `BAR_H`。

### 3.5 三条"占岛"路径对比

| 路径 | 形状 | 位置 | 生命周期 | 现成实现 |
|---|---|---|---|---|
| **A. 通知单槽**（`self.notice`） | 334×44（QQ/系统），含头像/标题/两行 + 「查看/关闭」按钮 | 展开区或收起态 44 宽窄条（`compact_notices()`） | `duration` 秒级，队列 8 条 | `draw_notice`（`extras.rs:476-603`），`messenger` 分支 `:478-489`、`External` 分支 `:490-500` |
| **B. HUD**（`Internal` + `is_hud()`） | 260×38，右侧数值 + 音量滑条 | 与 A 同槽 | 2.5s | `extras.rs:533-572` + `:617-654`（`HUD_TRACK_X=48` `HUD_TRACK_W=150`，`app.rs:47-48`） |
| **C. 会议大卡** | **360×136**：标题 + 倒计时胶囊 + 分隔线 + 双按钮 | 同槽 | 12s | `extras.rs:501-532` |
| **D. 收起态岛角标**（**唯一常驻+可实时更新**） | 药丸左侧：`[进度环][173 t/s]`，预留宽 96 / 24 / 64 三档 | 药丸内左侧，`left = local_cx - w.pos/2 + 8` | 常驻，跟着数据每帧重画 | `app.rs:3029-3060` + `island_badge_reserve()` `app.rs:1800-1812` + `task_ui.rs:190-192` |

### 3.6 「新下载任务以可持续更新的卡片（进度条 + 速度）上岛」——最贴近的现成路径

**推荐组合：C（会议大卡形状 + 自定义绘制）作为卡片外形，D 的"每帧读活数据"作为刷新机制，A 的 `enqueue_alert` 作为入场/抢占。**

理由（均有代码依据）：
1. `Alert` 字段是快照，但 `draw_notice(&mut self, c, panel, a)` **能访问 `&mut self`**（`extras.rs:476`），所以新分支可以每帧从 `self.download` 直接取最新进度/速度，不必每秒重建 `Alert`。
2. 入场/抢占完全复用 `enqueue_alert`（`extras.rs:286-312`）：新任务出现 → 入队；同一下载的进度刷新 → 走 `same_event` 判重后覆盖 `self.notice`（`extras.rs:298-306`），不会刷屏。
3. 卡片宽度 `notice_size()` 加一档即可（`app.rs:1815-1818`），例如 `(334.0, 64.0)` 放进度条 + 速度行；`HUD_TRACK_X/HUD_TRACK_W`（`app.rs:47-48`）+ `nc.fill(...)`（`extras.rs:643-650`）就是现成的进度条画法（音量条），直接照搬。
4. 收起态可以同时复用 D：把 `island_badge_reserve()`（`app.rs:1800-1812`）扩展成"下载优先于 AI 任务"，就能在药丸上显示 `[环][3.2 MB/s]`，与 AI 任务角标二选一。

需要新增的最小改动（不改既有语义）：
- `AlertSource` 加 `Download`（`extras.rs:9`）→ 同步 `tick_alerts` 的 `allowed` 闭包（`extras.rs:446-450`，需要 `self.settings.download_island && visible_pages[DOWNLOAD_PAGE]`）
- `Alert` 加 `download: Option<DownloadId>`（`extras.rs:33-44`），并在 `same_event`（`:298-301`）里按 id 判重
- `notice_size` 加尺寸档（`app.rs:1815-1818`）
- `draw_notice` 加分支（放在 `messenger` 分支之后、`External` 之前，`extras.rs:490` 之前）
- **收起规则**：沿用 `at/duration`，但让 `duration` 随进度"续期"（每次进度更新把 `a.at = self.t`），或对未完成的下载直接 `duration = f64::INFINITY`，完成/失败时 `self.notice=None`（`extras.rs:467` 那行是唯一清空点，语义要说明白）
- `compact_notices()`（`app.rs:1743`）目前 `meeting.is_some()` 会阻止紧凑化，下载卡若也想保留全宽，同样要把 `download.is_some()` 加进这个 `is_some_and(...)`

---

## 4. 交互动作：打开文件 / 打开目录 / 删除

| 动作 | 现成实现 | 证据 | 可否直接用 |
|---|---|---|---|
| 打开文件（默认程序） | `fn open_path(p:&Path)->Result<(),String>` — `ShellExecuteExW` + `w!("open")` + `SEE_MASK_FLAG_NO_UI` + `SW_SHOWNORMAL`；`ERROR_NO_ASSOCIATION` 时回落 `SHOpenWithDialog(OAIF_EXEC)` | `src/main.rs:996-1008`（`open_path_result` `:1010-1021`） | ✅ 直接用，但在 **main.rs（bin）** 里，库侧只能通过 `Cmd::Open(PathBuf)`（`app.rs:190`）→ `main.rs:736` |
| 打开所在文件夹并选中 | `Cmd::Reveal(p)` → `explorer.exe /select,<path>` | `src/main.rs:763` | ✅ 直接用（`Cmd::Reveal`，`app.rs:197`） |
| 打开文件并高亮会话记录 | `Cmd::OpenTask(source, log)` → 先 `focus_process_window`，失败回落 `explorer /select` | `src/main.rs:764-777`（`native.rs:342`） | ✅ 参考写法 |
| 打开 URL | `Cmd::OpenUrl` → `mail::web_url` 校验 → `open_path` | `src/main.rs:737-741`（`Cmd::OpenUrl`，`app.rs:194`） | ✅ |
| 聚焦已有窗口 | `daozai::native::focus_process_window(exe_name)->bool` | `src/native.rs:342` | ✅ 对 IDM/FDM 可用（`"IDMan.exe"` 等） |
| **删除文件到回收站** | **不存在**。全仓无 `SHFileOperation` / `IFileOperation` / `RecycleBin`；`std::fs::remove_file` 只出现在测试与缓存清理（`api_quota.rs:72`、`resources.rs:120`、`renderer.rs:33`） | grep 结果 | ❌ **必须新写** |
| 文件夹里已有"回收站"概念 | 仅在邮件模块（IMAP 文件夹）与 `mail_reader.rs:104` | `mail_manage.rs:1`、`pages.rs:1115` | 与本地文件无关，别误用 |

新写"删除到回收站"的可行路径 `[Assumed]`：`Cargo.toml` 已启用 `windows 0.58` 的 `Win32_UI_Shell` + `Win32_Storage_FileSystem`（`Cargo.toml` `[target.'cfg(windows)'.dependencies]`），因此可以直接：
1. `IFileOperation` + `FOF_ALLOWUNDO | FOF_NOCONFIRMATION`（推荐，COM 已启用 `Win32_System_Com`）；
2. 或 `SHFileOperationW` + `pFrom = "path\0\0"` + `FO_DELETE` + `FOF_ALLOWUNDO`。
**二次确认必须有**：`Cmd` 派发在外壳（`main.rs:720-810`），删除动作放在库里只能发 `Cmd::DeleteFile(PathBuf)`，由 `main.rs` 执行并 `say()` 回报。验证命令见 §7 R6。

---

## 5. 设置界面：新增一组开关要改哪里

### 5.1 先排除两个容易误认的文件

- `src/resident_settings.rs`（100 行）：**只有** `resident_order_settings()`，在「页面管理」页里给"常驻图标"（时间/电量/天气/AI 额度/音乐/专注/吉祥物）做拖拽排序，写 `self.settings.resident_order`（`:10-99`，`bar_layout::DEFAULT_ORDER`）。**不是页面开关。**
- `src/settings_window.rs`（147 行）：**只有**设置窗口的位置/大小持久化（`settings-window.json`、`Placement{position,size}`、`MIN_SIZE 600×450`、多显示器 DPI 适配）。**不含任何设置项 UI。**

### 5.2 设置 UI 真正的入口：`src/extras.rs` 的 `settings_ui`

```rust
// src/extras.rs:1487  pub fn settings_ui(&mut self, ctx:&egui::Context, autostart:bool)
// :1495  ctx.show_viewport_immediate(ViewportId::from_hash_of("daozai-settings"), ... 690×560 ...)
// :1518  左侧 SidePanel "settings-sidebar"（180 宽）→ 遍历 SETTINGS_GROUPS 画 14 项
// :1560  CentralPanel → match self.settings_tab { ... } 画右侧正文
// :2339  if before != format!("{:?}", self.settings) { self.settings.save(); }   ← 设置自动保存
```

新增「下载」相关开关的落点（对照 AI 任务页 `extras.rs:1575-1579`）：

| 位置 | 改什么 |
|---|---|
| `src/extras.rs:1441-1445` | `SETTINGS_GROUPS` 加 `("下载",16,16,"")`（在 `("AI 任务",15,15,"")` 之后） |
| `src/extras.rs:1538-1553` | 侧栏图标 `match start` 加 `16 => include_image!("../assets/settings-icons/system/download.svg")`（新 SVG 或复用 `tasks.svg`） |
| `src/extras.rs:1569` 起的 `match tab` | 加 `16=>{ let on=self.settings_feature_header(ui, DOWNLOAD_PAGE, "IDM / FDM / Ghost Downloader 最近下载", ...); ui.add_enabled_ui(on,|ui| self.download_settings(ui)); }` |
| `src/extras.rs:1285-1300` | `settings_page_icon(page)` 加 `DOWNLOAD_PAGE => ("download", include_bytes!(...))` |
| `src/extras.rs:1304-1307` | `settings_tab_module(tab)` 加 `16 => Some(DOWNLOAD_PAGE)` |
| `src/extras.rs:1602-1604` | 「页面管理」的 `descriptions` 数组追加第 11 项（**长度不足会越界 panic**） |
| `src/extras.rs:970-1010` 一带 | 复用 `setting_row` / `settings_card` / `settings_label` / `settings_segmented` / `settings_hint` 写 `download_settings(ui)`；样板 = `src/task_ui.rs:132-186` |
| `src/model.rs:290-299` + `:304` + `:399-405` + `:491-495` | 新字段（如 `download_sources:[bool;3]`、`download_island:bool`、`download_island_speed:bool`、`download_keep_minutes:u32`）四处齐全 |
| `src/app.rs:93-94` | `PAGE_NAMES` / `PAGE_SETTINGS_TABS` 各加一项 |

`quota_ui.rs` **不用改**（它只是 quota 页的渲染 + `page_quota`/`quota_settings` 实现，`quota_ui.rs:358` 只是读 `visible_pages[6]`）。

---

## 6. 测试与构建

### 6.1 两个 .cmd 各做什么（实测内容）

| 脚本 | 命令 | 用途 |
|---|---|---|
| `编译测试版-Claude.cmd` | `cargo +stable-x86_64-pc-windows-msvc build --locked --release --bin daozai --target x86_64-pc-windows-msvc`（`CARGO_BUILD_JOBS=1`，日志 `target\claude-skin\build.log`，末尾追加 `rustup show active-toolchain`） | **只编译**本机测试版，不跑测试 |
| `编译并测试-Claude.cmd` | 同上 build，然后 `cargo test --locked --all-targets --features record --target x86_64-pc-windows-msvc -- --test-threads=1`（日志 `target\claude-check\build.log`，两步 exit code 分别记录、任一非 0 就 `exit /b`，末尾 `pause`） | **正式编译 + 全量回归**（含 `record` bin、含 Windows 专属测试、单线程跑以防 GPU/全局状态竞争） |

两个脚本都会 `chcp 65001` 并 `cd /d "%~dp0"`，找不到 `cargo` 时回落到 `%USERPROFILE%\.cargo\bin\cargo.exe`。

### 6.2 release 产物路径

`target/x86_64-pc-windows-msvc/release/daozai.exe`（`AGENTS.md`：交付时覆盖桌面「岛仔－当前测试版」快捷方式指向的这个文件，并核对快捷方式目标与哈希；若岛仔在运行，切换后核对进程路径）。
`[profile.release] opt-level=3, lto=true, codegen-units=1, strip=true, panic="abort"`（`Cargo.toml`）→ **LTO + 单 codegen unit 意味着 release 编译慢，迭代时应先用 `cargo check` / `cargo test --lib`**。

### 6.3 ui_tests.rs 的测试风格

- 头部注释：「无窗口交互测试：直接向 egui 注入输入事件，验证悬停展开、切页、按钮」（`src/ui_tests.rs:1`）。
- 手写 harness `H::new()`：构造 `App` + `egui::Context`，`h.frame(vec![事件])` 推进一帧，收集 `h.shapes`；`h.cursor` / `h.hover(pos,dt)` / `h.wheel()` 注入指针；`ENV_LOCK` 互斥全局环境（`ui_tests.rs:18`）。
- **断言方式：结构断言，不是像素快照**——从 `output.shapes` 里捞 `egui::Shape::Text` 比对文案（如 `ui_tests.rs:28-36`），或断言 `h.app.pending` 里出现了某个 `Cmd`（`:41`），或断言状态机字段。**没有 golden/快照测试**，`image::save_buffer(...png)` 只在 `DAOZAI_GEMINI_PREVIEW_DIR` 之类环境变量存在时"顺手导出预览"（`ui_tests.rs:47-52`、`doubao_ui_tests.rs:93/118`），或仅在 `#[ignore]` 的测试里写 `%TEMP%`（`task_ui.rs:island_badge_preview`）。
- 结论：**新增页面不会因为"画面变了"打破断言**，但会打破三类硬断言：
  1. 数组长度（`[true;10]`、`visible_pages=[false;10]`、`page_order=[...9 项...]`）
  2. 持久化字符串（`"visible_pages=1011111111"`、`"page_order=5,2,...,9"`）
  3. 结构完整性（`every_module_settings_tab_maps_to_its_page` 要求页面↔设置分页一一对应）
- 其他测试文件：`tests/` 下是集成测试与 PowerShell 脚本（`black_skin.rs`、`calendar_credentials.rs`、`lyrics_memory.rs`、`performance.rs`、`native_installer.ps1`、`smtc_fixture.ps1`、`update_installer.ps1`、`diagnostics_archive.ps1`），与页面无关。

---

## 7. 风险清单

按"最容易踩"排序，每条给症状 + 依据 + 规避。

### R1（最高）持久化迁移写漏 → 老用户配置被静默重置
- 症状：升级后 11 个页面全开、顺序乱；或 `visible_pages` 整条被丢弃回 `[true;11]`。
- 依据：`model.rs:386` 的守卫是 `(6..=10).contains(&v.len())`，11 字符直接不匹配 → 落到默认值；`model.rs:364-379` 没有 `len==10 → 11` 分支 → `page_order` 保持默认 `[0..9]` 且第 11 位是 0（**会指向「音乐」页两次**，`nav()`/`position()` 的 `unwrap()` 都假设每页只出现一次）。
- 规避：先写迁移分支 + 先改 `ui_tests.rs:716-732` 的升级测试（它已经覆盖 8→10 的路径，照抄一条 10→11）。

### R2 三张索引表不同步 → panic 或错页
- `PAGE_NAMES`（`app.rs:93`）、`PAGE_SETTINGS_TABS`（`app.rs:94`）、`descriptions`（`extras.rs:1604`）都按页面索引取值；少一项就是 `index out of bounds`（`descriptions[page]` 在 `extras.rs:1625` 被直接索引）。`dots: [Spring;10]`（`app.rs:768`）少改会在画页码豆时越界。
- 规避：三表 + `dots` + `SETTINGS_GROUPS` + `settings_tab_module` 一次性改完，然后跑 `every_module_settings_tab_maps_to_its_page`（`ui_tests.rs:9296`）。

### R3 岛上布局宽度被撑爆 / 与吉祥物、角标抢占
- 收起态总宽是 `(resident.target.width + island_badge_reserve()).min(WIN_W-32)`（`app.rs:1840`），角标预留只有 96/24/64 三档（`app.rs:54-58`、`:1806-1811`）；展开面板固定 `PW=419`（`app.rs:24`），通知卡最大 360（会议，`app.rs:1816`）。
- 下载卡若同时要"进度环 + 速度 + 文件名 + 两个按钮"，44 高的 `(334,44)` 一定不够；`(360,136)` 又和会议卡同宽，容易挤掉吉祥物（`pet_space()` 44，`app.rs:1797`）。
- 规避：新尺寸档单独算，并在 `app.rs:1800-1812` 明确"下载角标 vs AI 任务角标"的优先级（**同一时刻只能有一个占位**，否则预留宽重复计算 → 药丸抖动）。

### R4 性能：轮询线程 + 每帧重绘
- 现成节奏：`POLL_INTERVAL=2s` + 单飞线程（`task_scan.rs:29`、`:283-291`），UI 侧 `request_repaint_after(500ms)`（`task_ui.rs:67`）。若下载页改成"每秒扫描 IDM 的 .dat / FDM 的 SQLite"，会把单飞线程变成持续占用；且 `Watcher::tick` 只有在 `watching`（页面展开或设置停在该分页）时才起线程（`task_ui.rs:41-43`）——**下载页必须保留这个门控**，否则收起时也在后台读盘。
- 隐藏坑：IDM/FDM 的部分数据文件在写入时会被独占锁定，读失败要当作"跳过这一条"，不能反复重试（参照 `task_scan.rs:220-226` `demote_stale` 的容错风格）。
- 岛角标每帧重画（`app.rs:3029`）已是既有成本；新增"速度字符串格式化"每帧执行（`app.rs:3047` 就是这样），注意别在每帧做 syscall。

### R5 测试断言大面积破裂（预期内，但要一次改干净）
`ui_tests.rs:731-732`、`:3332`、`:3340`、`:3346-3348`、`:6140`、`:6186-6189`、`:9296-9303`、`settings_redesign_ui_tests.rs:43/68` 全部依赖 10/旧字符串，见 §1.3。漏改 = `编译并测试-Claude.cmd` 必挂。

### R6 删除到回收站没有现成实现，容易误写成永久删除
- 全仓无 `SHFileOperation`/`IFileOperation`（§4）。如果用 `std::fs::remove_file` 用户数据就**不可恢复**。
- 规避：新写 `IFileOperation`（`FOF_ALLOWUNDO`）或 `SHFileOperationW(FO_DELETE|FOF_ALLOWUNDO)`，二次确认 + `say()` 回报；`cargo check` 通过只能说明编译，**必须手工验证文件确实进了回收站**（`Shell.Application` 枚举或资源管理器查看）。

### R7 数据来源完全未知（IDM/FDM/Ghost Downloader）
- 仓库里没有任何这三个软件的读取代码（`[Observed]`：无相关模块）。这部分是**纯新增**，接口稳定性风险最高：`[Assumed]` 通常 IDM 在 `%APPDATA%\IDM\` 有任务数据、FDM 在 `%APPDATA%` 下用 SQLite、Ghost Downloader 是 Python 应用写自己的配置目录，但**均未在本机验证**。
- 规避：先按 `task_scan.rs` 的"纯函数 `scan()` + 结构体 `DownloadItem`"分层，数据源用 trait/枚举隔离，把"读不到"当正常返回空；落地前用下面的探测命令把真实布局记录下来（写进本文件附录）。

---

## 附录 A：下载源探测（`Assumed`，落地前先跑）

> 并行调研旁证 `[Observed，但属于另一条调研线]`：仓库根已存在 `.probe/`（13 个探针脚本，21:05–21:09 生成，非本次调研产物），其中 `fdm_probe4.py` 直接指向
> `C:\Users\<user>\AppData\Local\Softdeluxe\Free Download Manager\db.sqlite`，并已确认它是 SQLite（读 `header[18]/[19]` 判 journal 模式、按 UTF-16**BE** 词表在 blob 里找 `speed / transferRate / bytesDownloaded / writeOffset` 之类的键）。
> 说明 FDM 的任务数据不在 `%APPDATA%`，而在 **`%LOCALAPPDATA%`**，且关键字段是 **UTF-16BE 编码的 protobuf 风格 blob**（不是普通行列表）。落地前请先合并 `.probe/` 的完整结论，别按 `%APPDATA%` 猜路径。

```powershell
# IDM
reg query "HKCU\Software\DownloadManager" /s | Select-String -Pattern "Folder|DwnlData|Path"
Get-ChildItem "$env:APPDATA\IDM" -Recurse -Depth 2 -ErrorAction SilentlyContinue | Select-Object FullName,Length
# Free Download Manager（注意是 LOCALAPPDATA，见上）
Get-ChildItem "$env:LOCALAPPDATA\Softdeluxe\Free Download Manager" -Recurse -Depth 2 -ErrorAction SilentlyContinue | Select-Object FullName,Length
Get-ChildItem "$env:APPDATA\Softdeluxe" -Recurse -Depth 3 -ErrorAction SilentlyContinue | Select-Object FullName,Length
# Ghost Downloader
Get-ChildItem "$env:APPDATA","$env:LOCALAPPDATA" -Filter "*Ghost*" -ErrorAction SilentlyContinue | Select-Object FullName
Get-Process | Where-Object { $_.ProcessName -match "IDMan|fdm|Ghost" } | Select-Object ProcessName,Path
```
每条的结论（文件格式、字段、是否需要锁/只读拷贝）都应以 `Observed` 形式补回本文件，再决定扫描策略。
**FDM 的两个实测注意点**：① 数据库可能被客户端独占锁 → 必须"复制到 temp 再打开"（`fdm_probe4.py:1` 就是 `shutil.copyfile` 后连副本）；② 字段名是 UTF-16BE，用 ASCII 搜索会一无所获。

---

## 附录 B：改动顺序建议（checklist）

1. `src/download_scan.rs`：`DownloadItem` + `Source` 枚举 + 纯函数 `scan()` + `Watcher`（照抄 `task_scan.rs:229-292`）+ 单测。
2. `src/model.rs`：`visible_pages`/`page_order` 长度；`download_*` 字段（结构体 + Default 单行 + load + save）；**迁移分支 10→11**。
3. `src/app.rs`：`DOWNLOAD_PAGE` 常量、`DOWNLOAD_H`、`PAGE_NAMES`、`PAGE_SETTINGS_TABS`、`dots`、`render_page`、`page_title`、`expanded_height`、`arrow_y`、`App.download` 字段 + 初始化、`self.download_tick()` 调用点（`:1878` 旁）。
4. `src/download_ui.rs`：`page_download` / `download_tick` / `download_settings` / `island_badge`（照抄 `task_ui.rs` 同名函数）。
5. `src/extras.rs`：`AlertSource::Download`、`Alert.download`、`enqueue_alert` 的 `same_event`、`tick_alerts` 的 `allowed`、`draw_notice` 新分支、`notice_size` 联动、`SETTINGS_GROUPS`/`descriptions`/`settings_page_icon`/`settings_tab_module`/分页 `match`。
6. `src/app.rs`：`island_badge_reserve` 优先级（下载 vs AI 任务）、`notice_size` 新尺寸档、`compact_notices`。
7. `src/lib.rs`：`pub mod download_scan; mod download_ui;`。
8. `src/main.rs`：`Cmd::DownloadOpen / DownloadReveal / DownloadDelete`（前者复用 `open_path`/`explorer /select`，删除新写 `IFileOperation`）。
9. 测试：改 §1.3 全部硬断言 + 新页专属测试（照 `ui_tests.rs:9305-9334` 的 AI 任务页测试结构）。
10. 构建验证：`编译测试版-Claude.cmd` → 手工点检 → `编译并测试-Claude.cmd` → 覆盖 `target/x86_64-pc-windows-msvc/release/daozai.exe` 并核对哈希。
