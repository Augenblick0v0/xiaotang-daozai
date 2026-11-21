//! 扫描本机三个 AI 编码客户端（DeepSeek Harness / Codex CLI / Claude Code）的本地会话日志，
//! 汇总成「正在跑 / 等你 / 刚结束」的任务列表，供岛上任务页绘制。
//! 全程只读：不写、不锁、不改任何会话文件；坏行、坏文件只跳过，绝不 panic。

use crate::poll_clock::PollClock;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// 列表最多画几条。
const MAX_ITEMS: usize = 12;
/// 标题最多留多少个字符（按 chars，不按字节）。
const TITLE_CHARS: usize = 34;
/// JSONL 只读尾部这么多字节，往前的历史不碰（Codex 单文件可达几十 MB）。
const TAIL_BYTES: u64 = 256 * 1024;
/// 头部读多少字节：会话元信息（cwd、模型、首条提问）都在文件开头，尾部读不到。
const HEAD_BYTES: u64 = 128 * 1024;
/// 单行超过这个长度直接跳过：宁可漏一条事件，也不让一行大 base64 拖慢整轮扫描。
const LINE_MAX_BYTES: usize = 8 * 1024 * 1024;
/// DSH 的 turn 还开着但没有「正在跑工具」的边界信息时，多久没活动就从「调用工具」降级成「正在思考」。
const FRESH_MS: i64 = 10_000;
/// 超过这么久没有任何活动，就不再把那轮会话当成「在跑」（100ns 不适用，这里是毫秒）。
const STALE_MS: i64 = 120 * 60_000;
/// 后台扫描节拍。
const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Dsh,
    Codex,
    Claude,
}

impl Source {
    pub fn key(self) -> &'static str {
        match self {
            Source::Dsh => "dsh",
            Source::Codex => "codex",
            Source::Claude => "claude",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::Dsh => "DeepSeek Harness",
            Source::Codex => "Codex",
            Source::Claude => "Claude Code",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Source::Dsh => "DSH",
            Source::Codex => "Codex",
            Source::Claude => "Claude",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Source::Dsh => 0,
            Source::Codex => 1,
            Source::Claude => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Starting,
    Thinking,
    Tool,
    WaitingInput,
    WaitingApproval,
    Done,
    Failed,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Starting => "启动中",
            Stage::Thinking => "正在思考",
            Stage::Tool => "调用工具",
            Stage::WaitingInput => "等你回复",
            Stage::WaitingApproval => "等你确认",
            Stage::Done => "已完成",
            Stage::Failed => "出错了",
        }
    }

    /// 需要人动手（列表里排最前、UI 打琥珀色）。
    pub fn needs_user(self) -> bool {
        matches!(self, Stage::WaitingInput | Stage::WaitingApproval)
    }

    /// 还在跑。
    pub fn alive(self) -> bool {
        matches!(self, Stage::Starting | Stage::Thinking | Stage::Tool)
    }

    /// 已经收场。
    pub fn terminal(self) -> bool {
        matches!(self, Stage::Done | Stage::Failed)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskItem {
    /// 稳定唯一：`源前缀:会话 id`，例如 `"dsh:09613bbe-…"`。
    pub id: String,
    pub source: Source,
    /// 会话标题，已压空白、去首尾、最多 200 字符。
    pub title: String,
    /// 工作目录名（cwd 最后一段），拿不到给空串。
    pub project: String,
    pub model: String,
    pub stage: Stage,
    /// 0.0..=1.0；只有能真实算出来时才 `Some`。
    pub progress: Option<f32>,
    pub started_ms: i64,
    pub updated_ms: i64,
    pub output_tokens: u64,
    /// token/秒，拿不到 0.0。
    pub rate: f32,
    /// 对应日志文件（「在资源管理器里显示」用）。
    pub log: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Roots {
    pub dsh: PathBuf,
    pub codex: PathBuf,
    pub claude: PathBuf,
}

impl Roots {
    /// 真实路径：`%USERPROFILE%\.dsh`、`%USERPROFILE%\.codex`、`%USERPROFILE%\.claude`。
    pub fn real() -> Self {
        let home = home_dir();
        Self {
            dsh: home.join(".dsh"),
            codex: home.join(".codex"),
            claude: home.join(".claude"),
        }
    }
}

impl Default for Roots {
    fn default() -> Self {
        Self::real()
    }
}

pub struct ScanConfig {
    /// 按 `Source::index()` 取；关掉的源连目录都不读。
    pub sources: [bool; 3],
    /// 只保留最近 N 分钟内有活动的会话。
    pub keep_minutes: u32,
    /// 「现在」由调用方给，方便测试。
    pub now_ms: i64,
}

/// 纯函数、无副作用、可在测试里指向临时目录。
pub fn scan(roots: &Roots, cfg: &ScanConfig) -> Vec<TaskItem> {
    let cutoff = cfg.now_ms.saturating_sub(cfg.keep_minutes as i64 * 60_000);
    let mut items: Vec<TaskItem> = Vec::new();
    if cfg.sources[Source::Dsh.index()] {
        items.extend(scan_dsh(roots, cutoff, cfg.now_ms));
    }
    if cfg.sources[Source::Codex.index()] {
        items.extend(scan_codex(roots, cutoff));
    }
    if cfg.sources[Source::Claude.index()] {
        items.extend(scan_claude(roots, cutoff));
    }
    // 同一 id 只留一条（先到先得），再按「等你 → 在跑 → 收场，同档新的在前」排序。
    let mut seen = HashSet::new();
    items.retain(|it| seen.insert(it.id.clone()));
    for it in &mut items {
        // 标题洗成空串（会话 id 之类）时退回目录名，总比留一片空白强。
        if it.title.trim().is_empty() {
            it.title = if it.project.trim().is_empty() {
                "未命名会话".to_string()
            } else {
                it.project.clone()
            };
        }
    }
    items.sort_by(|a, b| {
        stage_rank(a.stage)
            .cmp(&stage_rank(b.stage))
            .then(b.updated_ms.cmp(&a.updated_ms))
    });
    items.truncate(MAX_ITEMS);
    items
}

/// 会话文件还热、可那一轮早就没人看了：再算「在跑」会一直显示「已跑 149 小时」。
/// 这里只是不把它当成活着（仍然按「已结束」留着，等你点开看一眼）——
/// 放在扫描之后单独做，纯解析函数 `scan` 就仍然是对时间点无偏的。
pub fn demote_stale(items: &mut [TaskItem], now_ms: i64) {
    for it in items.iter_mut() {
        if it.stage.alive() && now_ms.saturating_sub(it.updated_ms) > STALE_MS {
            it.stage = Stage::Done;
        }
    }
}

/// 每 2 秒在后台线程扫一次，结果通过通道回传，UI 线程永不阻塞。
pub struct Watcher {
    items: Vec<TaskItem>,
    /// `inject` 塞过条目就锁住，别再让后台扫描覆盖（预览/测试用）。
    manual: bool,
    clock: PollClock,
    rx: Option<Receiver<Vec<TaskItem>>>,
    roots: Roots,
}

impl Watcher {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            manual: false,
            clock: PollClock::default(),
            rx: None,
            roots: Roots::real(),
        }
    }

    pub fn items(&self) -> &[TaskItem] {
        &self.items
    }

    /// 直接把条目塞进来：渲染预览和测试用，正式路径一律走 `tick`。
    pub fn inject(&mut self, items: Vec<TaskItem>) {
        self.items = items;
        self.manual = true;
    }

    /// 每次先收上一次的结果；没有在跑的扫描且到点了就起一个后台线程。
    pub fn tick(&mut self, _now: Instant, cfg: ScanConfig) {
        if self.manual {
            return;
        }
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(mut items) => {
                    demote_stale(&mut items, cfg.now_ms);
                    self.items = items;
                    self.rx = None;
                }
                Err(TryRecvError::Empty) => {}
                // 线程死了（或 send 失败）：保留上一次结果，等下一拍重新起线程。
                Err(TryRecvError::Disconnected) => self.rx = None,
            }
        }
        if self.rx.is_some() || !self.clock.ready(POLL_INTERVAL) {
            return;
        }
        self.clock = PollClock::now();
        let (tx, rx) = mpsc::channel();
        let roots = self.roots.clone();
        let spawned = std::thread::Builder::new()
            .name("daozai-tasks".to_string())
            .spawn(move || {
                // scan 内部不 unwrap、不索引越界，所以这里不会 panic 把结果弄丢。
                let items = scan(&roots, &cfg);
                let _ = tx.send(items);
            });
        if spawned.is_ok() {
            self.rx = Some(rx);
        }
    }
}

impl Default for Watcher {
    fn default() -> Self {
        Self::new()
    }
}

fn stage_rank(stage: Stage) -> u8 {
    if stage.needs_user() {
        0
    } else if stage.alive() {
        1
    } else {
        2
    }
}

/* ---------- 通用小工具 ---------- */

/// 标题：连续空白/换行压成单个空格，去掉提问常见的 markdown 前缀，最多 `TITLE_CHARS` 字符。
/// 会话里能拿来当标题的往往是一整段提问，直接摊在岛上会挤满两行，所以这里先洗一遍。
fn clean_title(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(4 * TITLE_CHARS));
    let mut pending_space = false;
    let mut count = 0usize;
    for ch in raw.chars() {
        if ch == '\n' || ch == '\r' {
            // 只取第一段有内容的行：后面的换行段落通常已经是提问正文了。
            if !out.is_empty() {
                break;
            }
            continue;
        }
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        // 行首的引用/列表/标题装饰对标题没有意义。
        if out.is_empty() && matches!(ch, '#' | '>' | '-' | '*' | '·' | '`') {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
            count += 1;
        }
        if count >= TITLE_CHARS {
            break;
        }
        out.push(ch);
        count += 1;
    }
    let out = out.trim_end().to_string();
    // 第一句就是一个链接时（「https://github.com/… 看下这个」），链接本身当标题没有信息量。
    let out = if out.starts_with("http://") || out.starts_with("https://") {
        let rest: Vec<&str> = out.split_whitespace().skip(1).collect();
        if rest.is_empty() {
            return String::new();
        }
        rest.join(" ")
    } else {
        out
    };
    // 会话 id 当标题是最没用的一种：交给调用方退回项目名。
    if is_session_id_title(&out) {
        return String::new();
    }
    out
}

/// `session-6d4a6714-fd2f-4db1-95f1-079a40fc…` 这种是缓存文件名漏进来的，不是标题。
fn is_session_id_title(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("session-") else {
        return false;
    };
    let hex = rest.chars().filter(|c| *c != '-').count();
    rest.chars().all(|c| c.is_ascii_hexdigit() || c == '-') && hex >= 16
}

/// Claude Code 会往 transcript 里塞「伪 user 行」：斜杠命令、IDE 打开文件提示、
/// 本地命令回执（实测首条 user 常常就是这些）。它们不是用户提问，既不能当标题，
/// 也不该算「用户刚发话还没答」。
fn is_meta_user_line(text: &str) -> bool {
    const META: [&str; 8] = [
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<local-command",
        "<ide_",
        "<system-reminder",
        "<bash-",
        "<user-prompt-submit",
    ];
    let head = text.trim_start().to_ascii_lowercase();
    if head.starts_with('[') && text.trim_end().ends_with(']') {
        // `[Request interrupted by user for tool use]` 这类是 CLI 写进去的状态行。
        return true;
    }
    META.iter().any(|tag| head.starts_with(tag))
}

/// `D:\proj\demo` → `demo`；拿不到给空串。
fn last_segment(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn home_dir() -> PathBuf {
    for key in ["USERPROFILE", "HOME"] {
        if let Some(v) = std::env::var_os(key) {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    // Windows 上 USERPROFILE 正常都在，兜底拼 HOMEDRIVE + HOMEPATH。
    if let (Some(drive), Some(rest)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let mut path = PathBuf::from(drive);
        path.push(rest);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    PathBuf::new()
}

/// 文件最后修改时间（UTC 毫秒）。
fn file_mtime_ms(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).ok(),
        Err(_) => None,
    }
}

/// RFC3339 → UTC 毫秒。
fn rfc3339_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// unix 秒（整数或小数）→ 毫秒。
fn secs_to_ms(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n.saturating_mul(1000));
    }
    value
        .as_f64()
        .filter(|f| f.is_finite())
        .map(|f| (f * 1000.0) as i64)
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 形如 `3f2a1b4c-5d6e-7f80-9a1b-2c3d4e5f6071` 的 uuid 串？
fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

/// 文件名 stem 尾部的 uuid（`rollout-2026-11-20T21-42-28-<uuid>`）。
fn trailing_uuid(stem: &str) -> Option<String> {
    if stem.len() < 36 {
        return None;
    }
    let tail = stem.get(stem.len() - 36..)?;
    is_uuid(tail).then(|| tail.to_string())
}

/// 读文件尾部 `TAIL_BYTES` 字节；起点在文件中间时丢掉第一行残行。
fn read_tail(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return None;
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf).ok()?;
    // 中途截断可能切在多字节字符里，lossy 会把残片换成替换符，那一行本来也要丢。
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start == 0 {
        return Some(text);
    }
    let cut = text.find('\n')?;
    text.get(cut + 1..).map(str::to_string)
}

/// 读「头部 + 尾部」：头部有 `session_meta`/`turn_context`/首条提问，尾部有当前状态。
/// 单个 rollout 可能几十 MB，整读会拖住扫描线程，所以两头各取一段，中间省略。
fn read_window(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len <= HEAD_BYTES + TAIL_BYTES {
        let mut buf = Vec::with_capacity(len as usize);
        file.read_to_end(&mut buf).ok()?;
        return Some(String::from_utf8_lossy(&buf).into_owned());
    }
    let mut head = vec![0u8; HEAD_BYTES as usize];
    file.read_exact(&mut head).ok()?;
    let head = String::from_utf8_lossy(&head).into_owned();
    let Some(cut) = head.rfind('\n') else {
        return read_tail(path);
    };
    let tail = read_tail(path)?;
    Some(format!("{}\n{}", &head[..cut], tail))
}

/* ---------- DSH ---------- */

/// DSH 的 `record.rows[key]` 形如 `{"ver":N,"seq":N,"val":<真实值>}`，这里取 `val`。
fn dsh_row<'a>(rows: &'a Value, key: &str) -> Option<&'a Value> {
    rows.get(key)?.get("val")
}

fn scan_dsh(roots: &Roots, cutoff_ms: i64, now_ms: i64) -> Vec<TaskItem> {
    let dir = roots
        .dsh
        .join("storages")
        .join("session_projcache")
        .join("sessions");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }
        if !entry.file_type().map(|k| k.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        // 先按 mtime 过滤：本机 340+ 个缓存文件里通常只有个位数是热的。
        let Some(mtime) = file_mtime_ms(&path) else {
            continue;
        };
        if mtime < cutoff_ms {
            continue;
        }
        let stem = Path::new(&name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let session_id = stem.strip_prefix("session-").unwrap_or(&stem).to_string();
        if session_id.is_empty() {
            continue;
        }
        if let Some(item) = parse_dsh(&roots.dsh, &path, &session_id, mtime, now_ms) {
            out.push(item);
        }
    }
    out
}

fn parse_dsh(
    dsh: &Path,
    path: &Path,
    session_id: &str,
    mtime: i64,
    now_ms: i64,
) -> Option<TaskItem> {
    let raw = fs::read(path).ok()?;
    if raw.len() > 8 * 1024 * 1024 {
        return None;
    }
    let root: Value = serde_json::from_slice(&raw).ok()?;
    let record = root.get("record")?;
    let rows = record.get("rows")?;
    let identity = record.get("identity").unwrap_or(&Value::Null);

    let title_row = dsh_row(rows, "title").and_then(Value::as_str);
    // 注：DSH 的 title 行只存一句被截断的预览（十来个字），首个提问的全文在 titleInput 里。
    let title_input = dsh_row(rows, "titleInput")
        .and_then(|v| v.get("first"))
        .and_then(|v| v.get("text"))
        .and_then(Value::as_str);
    // `title` 行本身也可能就是一句长提问或一个链接（实测见过整条都是 github 地址），
    // 所以两处都要过一遍 `clean_title`，洗完是空的就交给调用方退回项目名。
    let title = non_empty(title_row)
        .map(|t| clean_title(&t))
        .filter(|t| !t.is_empty())
        .or_else(|| non_empty(title_input).map(|t| clean_title(&t)).filter(|t| !t.is_empty()))
        .unwrap_or_default();

    let metadata = dsh_row(rows, "sessionListMetadata");
    let blank = metadata
        .and_then(|v| v.get("blank"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let time_context = dsh_row(rows, "timeContext");
    let last_msg = time_context
        .and_then(|v| v.get("lastMessageTime"))
        .and_then(Value::as_i64);
    let created = identity.get("createdAt").and_then(Value::as_i64);
    let cwd = dsh_row(rows, "workingDirectory")
        .and_then(Value::as_str)
        .or_else(|| identity.get("cwd").and_then(Value::as_str))
        .unwrap_or("");
    let project = last_segment(cwd);

    // 从来没有过消息的空壳会话（只写了默认行）不占列表。
    if blank || (title.is_empty() && last_msg.is_none()) {
        return None;
    }

    let boundary = dsh_row(rows, "turnBoundary");
    let kind = boundary
        .and_then(|v| v.get("lastStepBoundary"))
        .and_then(|v| v.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let open_turn = boundary
        .and_then(|v| v.get("openTurnStartSeq"))
        .map(|v| !v.is_null())
        .unwrap_or(false);
    let turn_open = dsh_row(rows, "approvalState")
        .and_then(|v| v.get("turnOpen"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let question_active = dsh_row(rows, "userQuestions")
        .and_then(|v| v.get("questions"))
        .and_then(|v| v.get("active"))
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);

    let updated_ms = last_msg.map(|t| t.max(mtime)).unwrap_or(mtime);
    let started_ms = created.unwrap_or(mtime);

    // 推断（真实数据里没采到样本）：openStep.pendingCalls 里出现"等审批"字样的条目 ⇒ 在等你确认。
    let awaiting_approval = dsh_row(rows, "sessionStats")
        .and_then(|v| v.get("openStep"))
        .and_then(|v| v.get("pendingCalls"))
        .and_then(Value::as_object)
        .map(|calls| {
            calls
                .values()
                .any(|c| value_mentions(c, &["approval", "approve", "await", "pending"]))
        })
        .unwrap_or(false);

    let stage = if question_active {
        Stage::WaitingInput
    } else if awaiting_approval {
        Stage::WaitingApproval
    } else if kind == "start" {
        Stage::Tool
    } else if open_turn {
        // turn 没关但还不知道是不是在跑工具：刚有动静算调用工具，久没动静算思考。
        if now_ms.saturating_sub(updated_ms) <= FRESH_MS {
            Stage::Tool
        } else {
            Stage::Thinking
        }
    } else if kind == "end" {
        // 步骤收尾了：turn 也关了才算结束，还开着就是下一轮马上来。
        if turn_open {
            Stage::Thinking
        } else {
            Stage::Done
        }
    } else if turn_open {
        Stage::Thinking
    } else {
        Stage::Done
    };

    let progress = dsh_row(rows, "todos")
        .and_then(Value::as_array)
        .and_then(|list| {
            if list.is_empty() {
                return None;
            }
            let completed = list
                .iter()
                .filter(|t| t.get("status").and_then(Value::as_str) == Some("completed"))
                .count();
            Some(completed as f32 / list.len() as f32)
        });

    let model = dsh_row(rows, "modelSelection")
        .and_then(|v| v.get("lastUsed"))
        .and_then(|v| v.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let stats = dsh_row(rows, "sessionStats");
    // 会话可能已经开了好几天，「已跑」要按当前这一步算才看得懂。
    let started_ms = stats
        .and_then(|v| v.get("openStep"))
        .and_then(|v| v.get("startTime"))
        .and_then(Value::as_i64)
        .unwrap_or(started_ms);
    let output_tokens = dsh_row(rows, "tokenUsage")
        .and_then(|v| v.get("totals"))
        .and_then(|v| v.get("outputTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let rate = {
        let decode_ms = stats
            .and_then(|v| v.get("decodeMs"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let decode_tokens = stats
            .and_then(|v| v.get("decodeTokens"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if decode_ms <= 0.0 {
            0.0
        } else {
            let rate = decode_tokens as f32 / (decode_ms as f32 / 1000.0);
            if rate.is_finite() && rate >= 0.0 {
                rate
            } else {
                0.0
            }
        }
    };

    let log = dsh_log_path(dsh, session_id).unwrap_or_else(|| path.to_path_buf());
    Some(TaskItem {
        id: format!("{}:{}", Source::Dsh.key(), session_id),
        source: Source::Dsh,
        title: clean_title(&title),
        project,
        model,
        stage,
        progress,
        started_ms,
        updated_ms,
        output_tokens,
        rate,
        log: Some(log),
    })
}

/// DSH 的真实事件流在 `<.dsh>/sessions/<cwd-slug>/<会话目录>/session.v4.jsonl.zstd`；
/// 会话目录名有的带 `session-` 前缀、有的不带，所以两个都试。
fn dsh_log_path(dsh: &Path, session_id: &str) -> Option<PathBuf> {
    let root = dsh.join("sessions");
    let slugs = fs::read_dir(&root).ok()?;
    for slug in slugs.flatten() {
        if !slug.file_type().map(|k| k.is_dir()).unwrap_or(false) {
            continue;
        }
        for name in [session_id.to_string(), format!("session-{session_id}")] {
            let candidate = slug.path().join(name).join("session.v4.jsonl.zstd");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 递归查一个 JSON 片段里有没有出现指定关键词（大小写不敏感）。
fn value_mentions(value: &Value, needles: &[&str]) -> bool {
    match value {
        Value::String(s) => {
            let lower = s.to_ascii_lowercase();
            needles.iter().any(|n| lower.contains(n))
        }
        Value::Array(items) => items.iter().any(|v| value_mentions(v, needles)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            let key = k.to_ascii_lowercase();
            needles.iter().any(|n| key.contains(n)) || value_mentions(v, needles)
        }),
        _ => false,
    }
}

/* ---------- Codex CLI ---------- */

fn scan_codex(roots: &Roots, cutoff_ms: i64) -> Vec<TaskItem> {
    let root = roots.codex.join("sessions");
    let mut files = Vec::new();
    collect_rollouts(&root, 4, &mut files);
    let mut out = Vec::new();
    for path in files {
        let Some(mtime) = file_mtime_ms(&path) else {
            continue;
        };
        if mtime < cutoff_ms {
            continue;
        }
        if let Some(item) = parse_codex(&path, mtime) {
            out.push(item);
        }
    }
    out
}

fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 || out.len() >= 4096 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_rollouts(&entry.path(), depth - 1, out);
        } else if kind.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("rollout-") && name.to_ascii_lowercase().ends_with(".jsonl") {
                out.push(entry.path());
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TailKind {
    Tool,
    Thinking,
}

fn parse_codex(path: &Path, mtime: i64) -> Option<TaskItem> {
    // 只读尾部：单文件几十 MB，整读会卡住扫描线程。
    let text = read_window(path)?;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut title: Option<String> = None;
    let mut last_started_ms: Option<i64> = None;
    let mut last_complete: Option<(i64, Option<String>)> = None;
    let mut aborted_ms: Option<i64> = None;
    let mut tail_kind: Option<TailKind> = None;
    let mut last_ts_ms: Option<i64> = None;
    let mut last_response_ms: Option<i64> = None;
    let mut output_tokens: u64 = 0;
    let mut rate: f32 = 0.0;
    let mut prev_usage: Option<(i64, u64)> = None;
    let mut saw_event = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.len() > LINE_MAX_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(rfc3339_ms);
        if ts.is_some() {
            last_ts_ms = ts;
        }
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = value.get("payload").unwrap_or(&Value::Null);
        match kind {
            "session_meta" => {
                saw_event = true;
                session_id = session_id
                    .or_else(|| non_empty(payload.get("session_id").and_then(Value::as_str)));
                cwd = cwd.or_else(|| non_empty(payload.get("cwd").and_then(Value::as_str)));
            }
            "turn_context" => {
                saw_event = true;
                model = model.or_else(|| non_empty(payload.get("model").and_then(Value::as_str)));
                cwd = cwd.or_else(|| non_empty(payload.get("cwd").and_then(Value::as_str)));
            }
            "event_msg" => {
                saw_event = true;
                match payload.get("type").and_then(Value::as_str).unwrap_or("") {
                    "task_started" => {
                        let started = payload.get("started_at").and_then(secs_to_ms).or(ts);
                        if started.is_some() {
                            last_started_ms = started;
                        }
                    }
                    "task_complete" => {
                        let done = payload
                            .get("completed_at")
                            .and_then(secs_to_ms)
                            .or(ts)
                            .unwrap_or(mtime);
                        last_complete = Some((done, codex_error(payload)));
                    }
                    "turn_aborted" => aborted_ms = ts.or(Some(mtime)),
                    "token_count" => {
                        let total = payload
                            .get("info")
                            .and_then(|i| i.get("total_token_usage"))
                            .and_then(|t| t.get("output_tokens"))
                            .and_then(Value::as_u64);
                        if let Some(total) = total {
                            output_tokens = total;
                            if let Some(at) = ts {
                                if let Some((prev_at, prev_total)) = prev_usage {
                                    let dt = (at - prev_at) as f32 / 1000.0;
                                    // 太密的样本是同一拍重复，太稀的是跨了长空档，都不用算速度。
                                    if (0.25..=120.0).contains(&dt) && total >= prev_total {
                                        let delta = (total - prev_total) as f32;
                                        rate = delta / dt;
                                    }
                                }
                                prev_usage = Some((at, total));
                            }
                        }
                    }
                    "item_completed" => {
                        let item = payload.get("item").unwrap_or(&Value::Null);
                        let is_user = item
                            .get("type")
                            .and_then(Value::as_str)
                            .map(|t| t.eq_ignore_ascii_case("usermessage"))
                            .unwrap_or(false);
                        if is_user && title.is_none() {
                            title = codex_item_text(item);
                        }
                    }
                    _ => {}
                }
            }
            "token_usage_record" => {
                saw_event = true;
                if let Some(total) = payload
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    output_tokens = output_tokens.max(total);
                }
            }
            "response_item" => {
                saw_event = true;
                last_response_ms = ts.or(last_response_ms);
                match payload.get("type").and_then(Value::as_str).unwrap_or("") {
                    "reasoning" => tail_kind = Some(TailKind::Thinking),
                    "function_call" | "custom_tool_call" => tail_kind = Some(TailKind::Tool),
                    // 与规格一致：其余响应项（含 message / 工具输出）都按工具阶段算。
                    _ => tail_kind = Some(TailKind::Tool),
                }
            }
            _ => {}
        }
    }
    if !saw_event && title.is_none() {
        return None;
    }

    let complete_ms = last_complete.as_ref().map(|(at, _)| *at);
    // 窗口里可能只截到任务中段：没有 task_started 时，看收尾之后还有没有新的模型输出。
    let alive = match (last_started_ms, complete_ms) {
        (Some(started), Some(done)) => started > done,
        (Some(_), None) => true,
        (None, Some(done)) => last_response_ms.map(|at| at > done).unwrap_or(false),
        (None, None) => false,
    };
    let stage = if alive {
        match tail_kind {
            Some(TailKind::Thinking) => Stage::Thinking,
            _ => Stage::Tool,
        }
    } else if last_complete
        .as_ref()
        .map(|(_, error)| error.is_some())
        .unwrap_or(false)
    {
        Stage::Failed
    } else if aborted_ms
        .map(|at| at >= complete_ms.unwrap_or(i64::MIN))
        .unwrap_or(false)
    {
        Stage::Failed
    } else {
        Stage::Done
    };

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let id_tail = session_id
        .clone()
        .or_else(|| trailing_uuid(&stem))
        .unwrap_or(stem);
    let project = cwd.as_deref().map(last_segment).unwrap_or_default();
    let title = title
        .or_else(|| non_empty(Some(project.as_str())))
        .unwrap_or_default();

    Some(TaskItem {
        id: format!("{}:{}", Source::Codex.key(), id_tail),
        source: Source::Codex,
        title: clean_title(&title),
        project,
        model: model.unwrap_or_default(),
        stage,
        // 只有能算出真实进度时才给（Codex 的 rollout 里没有可用的分母）。
        progress: None,
        started_ms: last_started_ms.or(complete_ms).unwrap_or(mtime),
        updated_ms: last_ts_ms.unwrap_or(mtime),
        output_tokens,
        rate: if rate.is_finite() && rate >= 0.0 {
            rate
        } else {
            0.0
        },
        log: Some(path.to_path_buf()),
    })
}

fn codex_error(payload: &Value) -> Option<String> {
    match payload.get("error")? {
        Value::String(s) => non_empty(Some(s.as_str())),
        Value::Object(map) => map
            .get("message")
            .and_then(Value::as_str)
            .and_then(|s| non_empty(Some(s))),
        _ => None,
    }
}

/// `item.content[0].text`（真实 rollout 里 item.type 是 `"UserMessage"` / `"AgentMessage"`）。
fn codex_item_text(item: &Value) -> Option<String> {
    let content = item.get("content")?.as_array()?;
    for part in content {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            let text = clean_title(text);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/* ---------- Claude Code ---------- */

fn scan_claude(roots: &Roots, cutoff_ms: i64) -> Vec<TaskItem> {
    let root = roots.claude.join("projects");
    let Ok(projects) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for project in projects.flatten() {
        if !project.file_type().map(|k| k.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if !name.to_ascii_lowercase().ends_with(".jsonl") {
                continue;
            }
            // 只要 `<uuid>.jsonl` 这种会话文件；子代理产物（agent-*.jsonl / journal.jsonl）不算任务。
            let stem = Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !is_uuid(&stem) {
                continue;
            }
            if !file.file_type().map(|k| k.is_file()).unwrap_or(false) {
                continue;
            }
            let path = file.path();
            let Some(mtime) = file_mtime_ms(&path) else {
                continue;
            };
            if mtime < cutoff_ms {
                continue;
            }
            if let Some(item) = parse_claude(&path, &stem, mtime) {
                out.push(item);
            }
        }
    }
    out
}

fn parse_claude(path: &Path, stem: &str, mtime: i64) -> Option<TaskItem> {
    let text = read_window(path)?;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut first_user: Option<String> = None;
    // 最后一条「真人在说话」的时间：跑着的任务按它算「已跑」，会话早期的时间没意义。
    let mut last_user_ts: Option<i64> = None;
    let mut pending_tools: HashSet<String> = HashSet::new();
    let mut last_stop: Option<String> = None;
    let mut output_tokens: u64 = 0;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    // 末尾是不是一条「用户在说话」的行（新一轮已经发出、模型还没答）。
    let mut tail_is_user = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.len() > LINE_MAX_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(ts) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(rfc3339_ms)
        {
            first_ts = first_ts.or(Some(ts));
            last_ts = Some(ts);
        }
        session_id =
            session_id.or_else(|| non_empty(value.get("sessionId").and_then(Value::as_str)));
        cwd = cwd.or_else(|| non_empty(value.get("cwd").and_then(Value::as_str)));
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "ai-title" => {
                ai_title =
                    ai_title.or_else(|| non_empty(value.get("aiTitle").and_then(Value::as_str)));
            }
            "user" => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                let mut texts: Vec<String> = Vec::new();
                let mut is_tool_result = false;
                match message.get("content") {
                    Some(Value::String(s)) => texts.push(s.clone()),
                    Some(Value::Array(parts)) => {
                        for part in parts {
                            match part.get("type").and_then(Value::as_str).unwrap_or("") {
                                "text" => {
                                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                                        texts.push(t.to_string());
                                    }
                                }
                                "tool_result" => {
                                    is_tool_result = true;
                                    if let Some(id) =
                                        part.get("tool_use_id").and_then(Value::as_str)
                                    {
                                        pending_tools.remove(id);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                let joined = clean_title(&texts.join(" "));
                if !is_tool_result && !joined.is_empty() && !is_meta_user_line(&joined) {
                    tail_is_user = true;
                    if let Some(ts) = value
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(rfc3339_ms)
                    {
                        last_user_ts = Some(ts);
                    }
                    if first_user.is_none() {
                        first_user = Some(joined);
                    }
                }
            }
            "assistant" => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                model = model.or_else(|| non_empty(message.get("model").and_then(Value::as_str)));
                if let Some(stop) = non_empty(message.get("stop_reason").and_then(Value::as_str)) {
                    last_stop = Some(stop);
                }
                if let Some(parts) = message.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("tool_use") {
                            if let Some(id) = part.get("id").and_then(Value::as_str) {
                                pending_tools.insert(id.to_string());
                            }
                        }
                    }
                }
                if let Some(tokens) = message
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    output_tokens = output_tokens.saturating_add(tokens);
                }
                tail_is_user = false;
            }
            _ => {}
        }
    }
    if first_ts.is_none() && last_ts.is_none() {
        return None;
    }

    let stage = if !pending_tools.is_empty() {
        // 有 tool_use 还没有对应的 tool_result ⇒ 还在跑工具。
        Stage::Tool
    } else if tail_is_user {
        // 用户刚发话、模型还没答。
        Stage::Thinking
    } else {
        match last_stop.as_deref() {
            Some("end_turn") | Some("stop") | Some("stop_sequence") => Stage::Done,
            Some("tool_use") => Stage::Tool,
            _ => Stage::Thinking,
        }
    };

    let id_tail = session_id.unwrap_or_else(|| stem.to_string());
    let project = cwd.as_deref().map(last_segment).unwrap_or_default();
    let title = ai_title
        .or(first_user)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| project.clone());

    Some(TaskItem {
        id: format!("{}:{}", Source::Claude.key(), id_tail),
        source: Source::Claude,
        title: clean_title(&title),
        project,
        model: model.unwrap_or_default(),
        stage,
        // Claude 的 transcript 里没有可用的任务分母。
        progress: None,
        started_ms: last_user_ts.or(first_ts).or(last_ts).unwrap_or(mtime),
        updated_ms: last_ts.unwrap_or(mtime),
        output_tokens,
        rate: 0.0,
        log: Some(path.to_path_buf()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// 每个测试独占一个临时目录，跑完不删（temp 目录，系统会清）。
    fn temp_root(tag: &str) -> PathBuf {
        let unique = format!(
            "daozai-task-scan-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let root = std::env::temp_dir().join(unique);
        let _ = fs::create_dir_all(&root);
        root
    }

    fn all_on() -> [bool; 3] {
        [true, true, true]
    }

    fn write_file(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, body);
    }

    fn dsh_session(
        title: &str,
        todos: Value,
        kind: &str,
        open_turn: Option<i64>,
        now: i64,
    ) -> String {
        // 真实数据里 kind=start ⇒ turn 开着；kind=end ⇒ turn 已收。
        let turn_open = kind == "start" || open_turn.is_some();
        let rows = serde_json::json!({
            "title": {"ver": 1, "seq": 10, "val": title},
            "workingDirectory": {"ver": 1, "seq": 10, "val": "D:\\proj\\demo_project"},
            "modelSelection": {"ver": 3, "seq": 10, "val": {"lastUsed": {"provider": "sub", "model": "deepseek-v4.1-flash", "reasoningEffort": "high"}, "pending": null}},
            "turnBoundary": {"ver": 2, "seq": 10, "val": {"openTurnStartSeq": open_turn, "lastStepStartSeq": 10, "lastStepBoundary": {"kind": kind, "seq": 10}, "lastTurn": 1}},
            "approvalState": {"ver": 1, "seq": 10, "val": {"policy": "never", "turnOpen": turn_open}},
            "sessionStats": {"ver": 1, "seq": 10, "val": {"decodeMs": 5000, "decodeTokens": 1000, "openStep": null}},
            "tokenUsage": {"ver": 2, "seq": 10, "val": {"totals": {"outputTokens": 1234}}},
            "todos": {"ver": 2, "seq": 10, "val": todos},
            "timeContext": {"ver": 3, "seq": 10, "val": {"lastMessageTime": now}},
            "userQuestions": {"ver": 2, "seq": 10, "val": {"questions": {"active": [], "settled": []}}},
        });
        serde_json::json!({
            "version": 7,
            "record": {
                "identity": {"formatVersion": 4, "createdAt": now - 60_000, "cwd": "D:\\proj\\demo_project"},
                "rows": rows,
            }
        })
        .to_string()
    }

    fn dsh_roots(now: i64) -> (Roots, PathBuf) {
        let root = temp_root("dsh");
        let dsh = root.join("dsh");
        let file = dsh
            .join("storages")
            .join("session_projcache")
            .join("sessions")
            .join("session-09613bbe-d1a8-42eb-afec-59b27c83caa7.json");
        let todos = serde_json::json!([
            {"content": "读仓库结构", "status": "completed"},
            {"content": "写解析器", "status": "in_progress"},
            {"content": "补测试", "status": "pending"},
        ]);
        write_file(
            &file,
            &dsh_session("你是一个 Rust 工程师", todos, "start", Some(5), now),
        );
        let roots = Roots {
            dsh,
            codex: root.join("codex"),
            claude: root.join("claude"),
        };
        (roots, file)
    }

    #[test]
    fn dsh_running_task_reports_stage_progress_model_project() {
        let now = now_ms();
        let (roots, _) = dsh_roots(now);
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 30,
            now_ms: now,
        };
        let items = scan(&roots, &cfg);
        assert_eq!(items.len(), 1, "应当只产出一条任务：{items:?}");
        let item = &items[0];
        assert_eq!(item.source, Source::Dsh);
        assert_eq!(
            item.stage,
            Stage::Tool,
            "lastStepBoundary.kind=start 应判为调用工具"
        );
        assert_eq!(item.progress, Some(1.0 / 3.0));
        assert_eq!(item.model, "deepseek-v4.1-flash");
        assert_eq!(item.project, "demo_project");
        assert_eq!(item.title, "你是一个 Rust 工程师");
        assert_eq!(item.id, "dsh:09613bbe-d1a8-42eb-afec-59b27c83caa7");
        assert_eq!(item.output_tokens, 1234);
        assert!(item.rate > 0.0);
        assert!(item.log.is_some());
    }

    #[test]
    fn dsh_null_todos_gives_no_progress() {
        let now = now_ms();
        let root = temp_root("dsh-null-todos");
        let dsh = root.join("dsh");
        let file = dsh
            .join("storages")
            .join("session_projcache")
            .join("sessions")
            .join("session-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.json");
        write_file(
            &file,
            &dsh_session("没有清单的会话", Value::Null, "end", None, now),
        );
        let roots = Roots {
            dsh,
            codex: root.join("codex"),
            claude: root.join("claude"),
        };
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 30,
            now_ms: now,
        };
        let items = scan(&roots, &cfg);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].progress, None, "todos 为 null 时不许瞎估进度");
        assert_eq!(items[0].stage, Stage::Done);
    }

    fn codex_line(value: Value) -> String {
        format!("{value}\n")
    }

    fn codex_lines(extra_tail: &str) -> String {
        let mut body = String::new();
        body.push_str(&codex_line(serde_json::json!({
            "timestamp": "2026-11-20T13:42:31.135Z",
            "ordinal": 0,
            "type": "session_meta",
            "payload": {"session_id": "01a1f931-bd5c-7693-84d4-86ed8b87024a", "cwd": "D:\\proj\\demo_project", "cli_version": "0.167.0"}
        })));
        body.push_str(&codex_line(serde_json::json!({
            "timestamp": "2026-11-20T13:42:31.136Z",
            "ordinal": 1,
            "type": "turn_context",
            "payload": {"model": "gpt-6.2-sol", "effort": "high", "cwd": "D:\\proj\\demo_project"}
        })));
        body.push_str(&codex_line(serde_json::json!({
            "timestamp": "2026-11-20T13:42:31.500Z",
            "ordinal": 2,
            "type": "event_msg",
            "payload": {"type": "item_completed", "item": {"type": "UserMessage", "content": [{"type": "text", "text": "把首页改成暗色"}]}}
        })));
        body.push_str(&codex_line(serde_json::json!({
            "timestamp": "2026-11-20T13:42:31.600Z",
            "ordinal": 3,
            "type": "event_msg",
            "payload": {"type": "task_started", "started_at": 1_795_000_000, "turn_id": "t1"}
        })));
        body.push_str(&codex_line(serde_json::json!({
            "timestamp": "2026-11-20T13:43:00.000Z",
            "ordinal": 4,
            "type": "response_item",
            "payload": {"type": "function_call", "name": "shell"}
        })));
        body.push_str(&codex_line(serde_json::json!({
            "timestamp": "2026-11-20T13:43:01.000Z",
            "ordinal": 5,
            "type": "response_item",
            "payload": {"type": "function_call", "name": "shell"}
        })));
        body.push_str(extra_tail);
        body
    }

    fn codex_roots(tag: &str, body: &str) -> Roots {
        let root = temp_root(tag);
        let codex = root.join("codex");
        let file = codex
            .join("sessions")
            .join("2026")
            .join("11")
            .join("20")
            .join("rollout-2026-11-20T21-42-28-01a1f931-bd5c-7693-84d4-86ed8b87024a.jsonl");
        write_file(&file, body);
        Roots {
            dsh: root.join("dsh"),
            codex,
            claude: root.join("claude"),
        }
    }

    #[test]
    fn codex_running_then_done_then_failed() {
        let now = now_ms();
        // 1) 只有 task_started + 工具调用 ⇒ 在跑、阶段是调用工具。
        let roots = codex_roots("codex-alive", &codex_lines(""));
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 30,
            now_ms: now,
        };
        let items = scan(&roots, &cfg);
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(
            items[0].stage.alive(),
            "最后一个 task_started 晚于 task_complete 时算在跑"
        );
        assert_eq!(items[0].stage, Stage::Tool);
        assert_eq!(items[0].project, "demo_project");
        assert_eq!(items[0].title, "把首页改成暗色");
        assert_eq!(items[0].progress, None);

        // 2) 追加一条无 error 的 task_complete ⇒ 已完成。
        let done_tail = codex_line(serde_json::json!({
            "timestamp": "2026-11-20T13:44:00.000Z",
            "ordinal": 6,
            "type": "event_msg",
            "payload": {"type": "task_complete", "completed_at": 1_795_000_100, "duration_ms": 30000}
        }));
        let roots = codex_roots("codex-done", &codex_lines(&done_tail));
        let items = scan(&roots, &cfg);
        assert_eq!(items[0].stage, Stage::Done, "{items:?}");

        // 3) task_complete 带 error.message ⇒ 出错了。
        let failed_tail = codex_line(serde_json::json!({
            "timestamp": "2026-11-20T13:44:00.000Z",
            "ordinal": 6,
            "type": "event_msg",
            "payload": {"type": "task_complete", "completed_at": 1_795_000_100, "error": {"message": "stream disconnected"}}
        }));
        let roots = codex_roots("codex-failed", &codex_lines(&failed_tail));
        let items = scan(&roots, &cfg);
        assert_eq!(items[0].stage, Stage::Failed, "{items:?}");
    }

    #[test]
    fn codex_half_written_last_line_does_not_panic() {
        let now = now_ms();
        let mut body = codex_lines("");
        // 最后一行写到一半（进程还在写文件）。
        body.push_str("{\"timestamp\":\"2026-11-20T13:45:00.000Z\",\"ordinal\":6,\"type\":\"response_item\",\"payload\":{\"type\":\"func");
        let roots = codex_roots("codex-half", &body);
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 30,
            now_ms: now,
        };
        let items = scan(&roots, &cfg);
        assert_eq!(items.len(), 1, "半行不许把整个文件丢掉");
        assert_eq!(items[0].stage, Stage::Tool, "前面的完整行照常解析");
    }

    #[test]
    fn claude_finished_turn_is_done() {
        let now = now_ms();
        let root = temp_root("claude-done");
        let claude = root.join("claude");
        let file = claude
            .join("projects")
            .join("d--proj-demo")
            .join("11111111-2222-3333-4444-555555555555.jsonl");
        let mut body = String::new();
        body.push_str(
            &serde_json::json!({
                "type": "user",
                "timestamp": "2026-11-20T10:00:00.000Z",
                "isSidechain": false,
                "cwd": "D:\\proj\\demo_project",
                "sessionId": "11111111-2222-3333-4444-555555555555",
                "message": {"role": "user", "content": "把标题改短一点"}
            })
            .to_string(),
        );
        body.push('\n');
        body.push_str(
            &serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-11-20T10:00:05.000Z",
                "isSidechain": false,
                "cwd": "D:\\proj\\demo_project",
                "sessionId": "11111111-2222-3333-4444-555555555555",
                "message": {
                    "role": "assistant",
                    "model": "claude-sonnet-4-5",
                    "stop_reason": "end_turn",
                    "usage": {"output_tokens": 42},
                    "content": [{"type": "text", "text": "改好了"}]
                }
            })
            .to_string(),
        );
        body.push('\n');
        write_file(&file, &body);
        let roots = Roots {
            dsh: root.join("dsh"),
            codex: root.join("codex"),
            claude,
        };
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 30,
            now_ms: now,
        };
        let items = scan(&roots, &cfg);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].source, Source::Claude);
        assert_eq!(items[0].stage, Stage::Done);
        assert_eq!(items[0].title, "把标题改短一点");
        assert_eq!(items[0].project, "demo_project");
        assert_eq!(items[0].model, "claude-sonnet-4-5");
    }

    #[test]
    fn claude_pending_tool_use_is_tool_stage() {
        let now = now_ms();
        let root = temp_root("claude-tool");
        let claude = root.join("claude");
        let file = claude
            .join("projects")
            .join("d--proj-demo")
            .join("22222222-3333-4444-5555-666666666666.jsonl");
        let mut body = String::new();
        body.push_str(&serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-11-20T10:00:00.000Z",
            "cwd": "D:\\proj\\demo_project",
            "sessionId": "22222222-3333-4444-5555-666666666666",
            "message": {"role": "assistant", "model": "claude-sonnet-4-5", "stop_reason": "tool_use", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash"}]}
        }).to_string());
        body.push('\n');
        write_file(&file, &body);
        let roots = Roots {
            dsh: root.join("dsh"),
            codex: root.join("codex"),
            claude,
        };
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 30,
            now_ms: now,
        };
        let items = scan(&roots, &cfg);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].stage, Stage::Tool);
    }

    #[test]
    fn claude_meta_user_line_is_not_the_title() {
        // 真实 Claude 数据里首条 user 常是斜杠命令 / IDE 提示这类注入行，不能当标题。
        let now = now_ms();
        let root = temp_root("claude-meta");
        let claude = root.join("claude");
        let file = claude
            .join("projects")
            .join("d--proj-demo")
            .join("33333333-4444-5555-6666-777777777777.jsonl");
        let mut body = String::new();
        body.push_str(&serde_json::json!({
            "type": "user",
            "timestamp": "2026-11-20T10:00:00.000Z",
            "sessionId": "33333333-4444-5555-6666-777777777777",
            "cwd": "D:\\proj\\demo_project",
            "message": {"role": "user", "content": "<command-name>/goal</command-name> <command-message>goal</command-message>"}
        }).to_string());
        body.push('\n');
        body.push_str(&serde_json::json!({
            "type": "user",
            "timestamp": "2026-11-20T10:00:01.000Z",
            "sessionId": "33333333-4444-5555-6666-777777777777",
            "cwd": "D:\\proj\\demo_project",
            "message": {"role": "user", "content": [{"type": "text", "text": "开发一个课件整理 skill"}]}
        }).to_string());
        body.push('\n');
        body.push_str(&serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-11-20T10:00:09.000Z",
            "sessionId": "33333333-4444-5555-6666-777777777777",
            "cwd": "D:\\proj\\demo_project",
            "message": {"role": "assistant", "model": "claude-sonnet-4-5", "stop_reason": "end_turn", "usage": {"output_tokens": 7}, "content": [{"type": "text", "text": "好"}]}
        }).to_string());
        body.push('\n');
        write_file(&file, &body);
        let roots = Roots {
            dsh: root.join("dsh"),
            codex: root.join("codex"),
            claude,
        };
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 30,
            now_ms: now,
        };
        let items = scan(&roots, &cfg);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "开发一个课件整理 skill");
    }

    #[test]
    fn disabled_source_is_not_scanned_and_missing_dirs_are_harmless() {
        let now = now_ms();
        let (roots, _) = dsh_roots(now);
        // dsh 关掉：即便目录里有一条活会话，也不许产出 dsh 任务；
        // codex/claude 指向不存在的目录同样不许 panic。
        let cfg = ScanConfig {
            sources: [false, false, false],
            keep_minutes: 30,
            now_ms: now,
        };
        assert!(scan(&roots, &cfg).is_empty());
        let cfg = ScanConfig {
            sources: [false, true, true],
            keep_minutes: 30,
            now_ms: now,
        };
        assert!(scan(&roots, &cfg).is_empty());
        let missing = Roots {
            dsh: roots.dsh.join("没有这个目录"),
            codex: roots.codex.join("没有这个目录"),
            claude: roots.claude.join("没有这个目录"),
        };
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 30,
            now_ms: now,
        };
        assert!(scan(&missing, &cfg).is_empty());
    }

    #[test]
    fn keep_minutes_zero_drops_everything() {
        let now = now_ms();
        let (roots, _) = dsh_roots(now);
        // 现场写出来的文件 mtime≈now，keep_minutes=0 ⇒ 截止时间=now，全被滤掉。
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 0,
            now_ms: now + 1_000,
        };
        assert!(scan(&roots, &cfg).is_empty());
        let cfg = ScanConfig {
            sources: all_on(),
            keep_minutes: 30,
            now_ms: now + 1_000,
        };
        assert_eq!(
            scan(&roots, &cfg).len(),
            1,
            "同样的 fixture 在 30 分钟窗口里应当留下"
        );
    }

    #[test]
    fn sorting_prefers_waiting_then_alive_then_terminal() {
        let now = now_ms();
        let mut waiting = dsh_item(Stage::WaitingInput, now - 5_000);
        waiting.id = "dsh:waiting".into();
        let mut alive = dsh_item(Stage::Tool, now - 1_000);
        alive.id = "dsh:alive".into();
        let mut done = dsh_item(Stage::Done, now);
        done.id = "dsh:done".into();
        let mut items = vec![done, alive, waiting];
        items.sort_by(|a, b| {
            stage_rank(a.stage)
                .cmp(&stage_rank(b.stage))
                .then(b.updated_ms.cmp(&a.updated_ms))
        });
        assert_eq!(items[0].stage, Stage::WaitingInput);
        assert_eq!(items[1].stage, Stage::Tool);
        assert_eq!(items[2].stage, Stage::Done);
    }

    fn dsh_item(stage: Stage, updated: i64) -> TaskItem {
        TaskItem {
            id: String::new(),
            source: Source::Dsh,
            title: "t".into(),
            project: "p".into(),
            model: "m".into(),
            stage,
            progress: None,
            started_ms: updated,
            updated_ms: updated,
            output_tokens: 0,
            rate: 0.0,
            log: None,
        }
    }

    #[test]
    fn title_is_collapsed_and_truncated() {
        // 多段提问只取第一段：后面的段落已经是正文，摊到岛上没有意义。
        let long = format!("  a\n\n\tb   {}", "字".repeat(400));
        assert_eq!(clean_title(&long), "a");
        // 单行太长时按 TITLE_CHARS 截断，行内空白压成一个空格。
        let one_line = format!("a  b   {}", "字".repeat(400));
        let title = clean_title(&one_line);
        assert!(title.starts_with("a b 字"), "{title}");
        assert_eq!(title.chars().count(), TITLE_CHARS);
        assert_eq!(clean_title("  \n  "), "");
        // 会话 id、纯链接都不当标题，交给调用方退回项目名。
        assert_eq!(clean_title("session-6d4a6714-fd2f-4db1-95f1-079a40fc1234"), "");
        assert_eq!(clean_title("https://github.com/Renly1994/xiaotang-daozai"), "");
        assert_eq!(clean_title("https://example.com/a 看下这个"), "看下这个");
    }

    #[test]
    fn last_segment_handles_both_separators() {
        assert_eq!(last_segment("D:\\proj\\demo"), "demo");
        assert_eq!(last_segment("D:/proj/demo/"), "demo");
        assert_eq!(last_segment(""), "");
        assert_eq!(last_segment("demo"), "demo");
    }

    /// 真机实测用：只在手动指定时跑（`cargo test --lib task_scan -- --ignored --nocapture`）。
    #[test]
    #[ignore]
    fn live_scan() {
        let roots = Roots::real();
        // 默认 30 分钟窗口；想连历史数据一起看（比如 Claude 的旧会话）就设 DAOZAI_LIVE_KEEP=1000000。
        let keep = std::env::var("DAOZAI_LIVE_KEEP")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(30);
        let now = now_ms();
        let cfg = ScanConfig {
            sources: [true, true, true],
            keep_minutes: keep,
            now_ms: now,
        };
        let items = scan(&roots, &cfg);
        println!("roots = {roots:?}");
        println!("keep_minutes={keep} items={}", items.len());
        for it in &items {
            let progress = it
                .progress
                .map(|p| format!("{:.0}%", p * 100.0))
                .unwrap_or_else(|| "无".to_string());
            let idle_min = now.saturating_sub(it.updated_ms) / 60_000;
            println!(
                "{} {:<6} 进度={:<4} {:>6.1}t/s {:>7}tok 更新={}分钟前 | {} | {} | {} | {}",
                it.source.short(),
                it.stage.label(),
                progress,
                it.rate,
                it.output_tokens,
                idle_min,
                it.project,
                it.model,
                it.id,
                it.title
            );
            if let Some(log) = &it.log {
                println!("    log = {}", log.display());
            }
        }
    }
}
