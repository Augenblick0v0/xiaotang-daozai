//! 下载任务的只读采集：IDM / Free Download Manager / Ghost Downloader。
//!
//! 三个源都是「只读、不碰下载器本体」：IDM 读注册表，FDM 只读打开 sqlite，
//! Ghost 读它自己的 tasks.jsonl。任何一处读不到都只是「这一源没有数据」，
//! 绝不 panic、绝不写盘——本模块不产生任何副作用。
//!
//! 每个条目带 `live`：表示这个进度/速度是否**实时可信**（下载器进程在跑且该源支持）。
//! 为 false 时界面必须标成「上次记录」，别把落盘快照当实时。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use chrono::TimeZone;

use crate::poll_clock::PollClock;

/// 一次扫描的间隔。三个源合计最坏 ~30 ms，1.5 s 一拍完全够。
pub const POLL_INTERVAL: Duration = Duration::from_millis(1500);
/// 列表最多留多少条（页面只画 4 行，其余靠滚动；排序后截断）。
pub const MAX_ITEMS: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Idm,
    Fdm,
    Ghost,
}

impl Source {
    pub const ALL: [Source; 3] = [Source::Idm, Source::Fdm, Source::Ghost];

    pub fn index(self) -> usize {
        match self {
            Source::Idm => 0,
            Source::Fdm => 1,
            Source::Ghost => 2,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Source::Idm => "idm",
            Source::Fdm => "fdm",
            Source::Ghost => "ghost",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::Idm => "IDM",
            Source::Fdm => "FDM",
            Source::Ghost => "Ghost",
        }
    }

    /// 下载器主程序名：用于判断「进度是不是实时」。
    /// IDM 本机可能只剩 IDMan.exe.BAK（不常驻），那时按快照处理。
    pub fn exe(self) -> &'static str {
        match self {
            Source::Idm => "IDMan.exe",
            Source::Fdm => "fdm.exe",
            Source::Ghost => "Ghost-Downloader-3.exe",
        }
    }

    pub fn from_key(key: &str) -> Option<Source> {
        Source::ALL.into_iter().find(|s| s.key() == key)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DlState {
    /// 正在写盘。
    Downloading,
    Paused,
    /// 排队/等待。
    Queued,
    Failed,
    Done,
    /// 状态字段缺失或值不认识。
    Unknown,
}

impl DlState {
    pub fn label(self) -> &'static str {
        match self {
            DlState::Downloading => "下载中",
            DlState::Paused => "已暂停",
            DlState::Queued => "等待中",
            DlState::Failed => "失败",
            DlState::Done => "已完成",
            DlState::Unknown => "未知",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, DlState::Downloading | DlState::Paused | DlState::Queued)
    }

    /// 列表排序档位：活着的排前面，失败的其次，完成的最沉底。
    fn rank(self) -> u8 {
        match self {
            DlState::Downloading => 0,
            DlState::Paused => 1,
            DlState::Queued => 2,
            DlState::Failed => 3,
            DlState::Unknown => 4,
            DlState::Done => 5,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DownloadItem {
    /// `"<源>:<源内 id>"`，跨源唯一，点击命中与上岛去重都靠它。
    pub id: String,
    pub source: Source,
    /// 显示用文件名（不含目录）。
    pub name: String,
    /// 期望的本地完整路径；文件可能已被删或还没落到最终位置。
    pub path: PathBuf,
    /// 总字节数；0 = 未知。
    pub total: u64,
    /// 已下载字节数（超过 total 时按 total 夹一下，别画出 120%）。
    pub done: u64,
    /// 字节/秒；0 = 不可知。
    pub speed: u64,
    pub state: DlState,
    /// unix 毫秒；0 = 未知。
    pub created_ms: i64,
    pub finished_ms: Option<i64>,
    /// 失败原因、源内附注之类的一句话。
    pub detail: Option<String>,
    /// 进度/速度是否实时可信。
    pub live: bool,
}

impl DownloadItem {
    pub fn percent(&self) -> f32 {
        if self.total == 0 {
            return if self.state == DlState::Done { 1.0 } else { 0.0 };
        }
        (self.done.min(self.total) as f64 / self.total as f64) as f32
    }

    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    /// 「16.99 MB」或「1.2 MB / 16.99 MB」（未完成时给出已下载/总量）。
    pub fn size_text(&self) -> String {
        if self.total == 0 {
            return if self.done > 0 { bytes_text(self.done) } else { "大小未知".to_string() };
        }
        let total = bytes_text(self.total);
        if self.done > 0 && self.done < self.total {
            format!("{} / {}", bytes_text(self.done), total)
        } else {
            total
        }
    }

    /// 「3.4 MB/s」；不可知时给空串。
    pub fn speed_text(&self) -> String {
        if self.speed == 0 {
            String::new()
        } else {
            format!("{}/s", bytes_text(self.speed))
        }
    }

    /// 完成时间优先，没有完成时间给创建时间。本地时区，精确到分钟。
    pub fn time_text(&self) -> String {
        let ms = self.finished_ms.unwrap_or(self.created_ms);
        if ms <= 0 {
            return String::new();
        }
        match chrono::Local.timestamp_millis_opt(ms).single() {
            Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
            None => String::new(),
        }
    }

    /// 副行文案：「16.99 MB · 已完成 · 2026-11-21 20:22」。
    pub fn subtitle(&self, with_speed: bool) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(4);
        parts.push(self.size_text());
        parts.push(self.state.label().to_string());
        if with_speed {
            let s = self.speed_text();
            if !s.is_empty() {
                parts.push(s);
            }
        }
        let t = self.time_text();
        if !t.is_empty() {
            parts.push(t);
        }
        parts.join(" · ")
    }
}

/// 「512 B」「12.3 KB」「16.99 MB」「1.21 GB」。
pub fn bytes_text(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let f = n as f64;
    if f < KB {
        format!("{} B", n)
    } else if f < MB {
        format!("{:.1} KB", f / KB)
    } else if f < GB {
        format!("{:.2} MB", f / MB)
    } else {
        format!("{:.2} GB", f / GB)
    }
}

/// 三个源的数据位置。IDM 走注册表不需要路径，所以这里只有两个文件。
#[derive(Clone, Debug)]
pub struct Roots {
    pub fdm_db: PathBuf,
    pub ghost_jsonl: PathBuf,
}

impl Roots {
    pub fn real() -> Roots {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
        Roots {
            fdm_db: local.join("Softdeluxe").join("Free Download Manager").join("db.sqlite"),
            ghost_jsonl: local.join("GhostDownloader").join("tasks.jsonl"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScanConfig {
    /// 逐源的开关，下标即 `Source::index()`。
    pub sources: [bool; 3],
    /// 只留这么长时间内的任务（分钟）；0 = 不按时间过滤。
    pub keep_minutes: u32,
    /// 调用方给的「现在」（unix 毫秒），便于测试时固定时间。
    pub now_ms: i64,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig { sources: [true; 3], keep_minutes: 24 * 60, now_ms: now_ms() }
    }
}

pub fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// 下载器主程序是否在跑。不在跑时进度只是「上次落盘值」。
pub fn proc_alive(exe: &str) -> bool {
    !crate::smtc::player_process::pids_named(exe).is_empty()
}

/// 扫一遍三个源。纯读取：不写文件、不改注册表、不 panic。
pub fn scan(roots: &Roots, cfg: &ScanConfig) -> Vec<DownloadItem> {
    let mut items = Vec::new();
    if cfg.sources[Source::Idm.index()] {
        items.extend(parse_idm(cfg));
    }
    if cfg.sources[Source::Fdm.index()] {
        items.extend(parse_fdm(roots, cfg));
    }
    if cfg.sources[Source::Ghost.index()] {
        items.extend(parse_ghost(roots, cfg));
    }
    normalize(&mut items, cfg);
    items
}

/// 过滤 + 排序 + 截断。放在这里单独做，三个解析器就只管把字段填对。
fn normalize(items: &mut Vec<DownloadItem>, cfg: &ScanConfig) {
    if cfg.keep_minutes > 0 {
        let cutoff = cfg.now_ms.saturating_sub(cfg.keep_minutes as i64 * 60_000);
        // 创建时间未知（0）的条目不敢丢，留着。
        items.retain(|it| it.created_ms <= 0 || it.created_ms >= cutoff);
    }
    items.sort_by(|a, b| {
        a.state
            .rank()
            .cmp(&b.state.rank())
            .then(b.created_ms.cmp(&a.created_ms))
            .then(a.id.cmp(&b.id))
    });
    items.truncate(MAX_ITEMS);
}

/* ---------- 公共小工具 ---------- */

/// 注册表 API 只吃 NUL 结尾的 UTF-16。
#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 8 字节小端 u64。IDM 把 `FileSize`/`Downloaded` 存成 REG_NONE 的裸字节，
/// 长度不一定是 8（老版本只写 4 字节），所以按「有多少读多少」拼。
fn u64_le(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut v = 0u64;
    for (i, b) in bytes.iter().take(8).enumerate() {
        v |= (*b as u64) << (8 * i);
    }
    Some(v)
}

/// unix 秒 → 毫秒。Ghost 有可能写毫秒，两种都收（> 1e11 一定是毫秒）。
fn unix_to_ms(v: i64) -> i64 {
    if v <= 0 {
        0
    } else if v > 100_000_000_000 {
        v
    } else {
        v.saturating_mul(1000)
    }
}

/// 只取文件名部分：下载器存的可能是 `name?query`、带目录的路径、或 URL 末段。
fn file_name(raw: &str) -> String {
    let s = raw.split(|c: char| c == '?' || c == '#').next().unwrap_or("");
    let s = s.rsplit(|c: char| c == '\\' || c == '/').next().unwrap_or("");
    s.trim().to_string()
}

/// 从 URL 末段猜文件名；末段没有扩展名就不当作文件名（`https://site/` 这种）。
fn url_file_name(url: &str) -> String {
    let base = url.split(|c: char| c == '?' || c == '#').next().unwrap_or("");
    let last = base.rsplit('/').next().unwrap_or("").trim();
    if last.is_empty() || !last.contains('.') {
        return String::new();
    }
    last.to_string()
}

/// IDM 的时间字符串形如 `Apr 12 15:15:47 2025`（本地时间，日期可能只占一格宽度）。
/// 自己拆字段而不套格式串，是因为 `%e`/`%d` 对空格补齐的处理和 IDM 写出来的不一致。
fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 4 {
        return None;
    }
    let month = match parts[0].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let day: u32 = parts[1].parse().ok()?;
    let year: i32 = parts[3].parse().ok()?;
    let mut hms = parts[2].split(':');
    let h: u32 = hms.next()?.parse().ok()?;
    let m: u32 = hms.next()?.parse().ok()?;
    let s: u32 = hms.next()?.parse().ok()?;
    let naive = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(h, m, s)?;
    // 夏令时空档/重复时 `earliest()` 总能给一个确定值，别让整条任务因此没有时间。
    let local = chrono::Local.from_local_datetime(&naive).earliest()?;
    Some(local.timestamp_millis())
}

/* ---------- IDM（注册表） ---------- */

#[cfg(windows)]
mod idm {
    use super::*;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, REG_NONE, REG_VALUE_TYPE,
    };

    const ROOT: &str = "Software\\DownloadManager";

    /// 打开的子键，`Drop` 时关掉：345 个任务键要是漏关，句柄一会儿就耗光。
    struct Key(HKEY);

    impl Key {
        fn open(parent: HKEY, sub: &str) -> Option<Key> {
            let sub = wide(sub);
            let mut h = HKEY::default();
            let rc = unsafe {
                RegOpenKeyExW(
                    parent,
                    PCWSTR(sub.as_ptr()),
                    0,
                    KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS,
                    &mut h,
                )
            };
            (rc.0 == 0).then_some(Key(h))
        }
    }

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    /// 值的原始字节 + 类型。先问一次长度再取，省得猜缓冲区大小。
    fn read_raw(key: HKEY, name: &str) -> Option<(REG_VALUE_TYPE, Vec<u8>)> {
        let name = wide(name);
        let name = PCWSTR(name.as_ptr());
        let mut ty = REG_NONE;
        let mut len = 0u32;
        let rc = unsafe { RegQueryValueExW(key, name, None, Some(&mut ty), None, Some(&mut len)) };
        // 注册表值最大也就几 KB，离谱的长度说明拿到的不是我们以为的那个值。
        if rc.0 != 0 || len == 0 || len > 8192 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let rc = unsafe {
            RegQueryValueExW(key, name, None, Some(&mut ty), Some(buf.as_mut_ptr()), Some(&mut len))
        };
        if rc.0 != 0 {
            return None;
        }
        buf.truncate(len as usize);
        Some((ty, buf))
    }

    /// 取字符串。`REG_SZ` 和 `LocalPathW` 那种「REG_NONE 里塞 UTF-16」的都要能读，
    /// 所以不看类型，一律按 UTF-16LE 解。
    fn read_text(key: HKEY, name: &str) -> Option<String> {
        let (_ty, bytes) = read_raw(key, name)?;
        let units: Vec<u16> =
            bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let s = String::from_utf16_lossy(&units);
        let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        (!s.is_empty()).then(|| s.to_string())
    }

    fn read_u32(key: HKEY, name: &str) -> Option<u32> {
        let (_ty, bytes) = read_raw(key, name)?;
        (bytes.len() >= 4).then(|| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64(key: HKEY, name: &str) -> Option<u64> {
        let (_ty, bytes) = read_raw(key, name)?;
        u64_le(&bytes)
    }

    fn subkeys(key: HKEY) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = vec![0u16; 256];
        // 8192 只是防死循环的护栏：真实根键 363 个子键，其中 345 个是任务。
        for i in 0..8192u32 {
            let mut n = buf.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    key,
                    i,
                    PWSTR(buf.as_mut_ptr()),
                    &mut n,
                    None,
                    PWSTR::null(),
                    None,
                    None,
                )
            };
            if rc.0 != 0 {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]));
        }
        out
    }

    pub(super) fn parse() -> Vec<DownloadItem> {
        let Some(root) = Key::open(HKEY_CURRENT_USER, ROOT) else {
            return Vec::new();
        };
        // 最终保存目录只在根键上：`LocalPathW`（REG_NONE + UTF-16LE，带尾分隔符）。
        // 任务键里的 `LocalFileName`/`LocalPath` 是下载中的临时 .part，不是它。
        let dir = read_text(root.0, "LocalPathW").unwrap_or_default();
        let live = proc_alive(Source::Idm.exe());
        let mut out = Vec::new();
        for name in subkeys(root.0) {
            // 任务子键名就是纯数字 id；其它名字（Stats 之类）不是任务。
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            // 单个键读失败只丢这一条，不影响整轮。
            let Some(key) = Key::open(root.0, &name) else {
                continue;
            };
            if let Some(item) = task(key.0, &name, &dir, live) {
                out.push(item);
            }
        }
        out
    }

    fn task(key: HKEY, sid: &str, dir: &str, live: bool) -> Option<DownloadItem> {
        // 名字三级退让：FileName → URL 末段 → 编号。本机 345 个任务里有 5 个 FileName 是空串
        // （IDM 只留了 URL 的取消任务），它们 FileSize/Status 都齐，丢掉就等于少报一条真任务。
        let name = {
            let mut name = file_name(&read_text(key, "FileName").unwrap_or_default());
            if name.is_empty() {
                name = read_text(key, "Url0")
                    .or_else(|| read_text(key, "Url1"))
                    .map(|url| url_file_name(&url))
                    .unwrap_or_default();
            }
            if name.is_empty() {
                name = format!("下载 #{}", sid);
            }
            name
        };
        let total = read_u64(key, "FileSize").unwrap_or(0);
        let done = read_u64(key, "Downloaded").unwrap_or(0);
        let detail = read_text(key, "lastResult");
        let state = match read_u32(key, "Status") {
            // 3 = 已完成。实测 301 个任务全是 Downloaded == FileSize。
            Some(3) => DlState::Done,
            // 0 = 取消/出错/还没开始，具体哪种看 lastResult 有没有内容。
            Some(0) => {
                if detail.is_some() {
                    DlState::Failed
                } else {
                    DlState::Queued
                }
            }
            // 推断，无实测样本：本机 345 个任务的 Status 只有 0 和 3，没有 1/2。
            // 按 IDM 的语义，1 正在下一半、2 被暂停。
            Some(1) => DlState::Downloading,
            Some(2) => DlState::Paused,
            _ => DlState::Unknown,
        };
        let created_ms = created_ms(key);
        // IDM 不写完成时间；`lastTryDate` 是最后一次尝试的时刻，完成的任务它就等于完成时间。
        let finished_ms = if state == DlState::Done {
            Some(parse_date(&read_text(key, "lastTryDate").unwrap_or_default()).unwrap_or(created_ms))
                .filter(|ms| *ms > 0)
        } else {
            None
        };
        // `Speed` 是残留值（任务早结束了还留着旧速度），只有进程在跑时才敢采信。
        let speed = if live { read_u32(key, "Speed").unwrap_or(0) as u64 } else { 0 };
        let path = if dir.is_empty() { PathBuf::new() } else { PathBuf::from(dir).join(&name) };
        Some(DownloadItem {
            id: format!("{}:{}", Source::Idm.key(), sid),
            source: Source::Idm,
            name,
            path,
            total,
            done,
            speed,
            state,
            created_ms,
            finished_ms,
            detail,
            live,
        })
    }

    fn created_ms(key: HKEY) -> i64 {
        // `st_time` 是 unix 秒；老条目没有它就退回 `dateAdded` 的本地时间字符串。
        if let Some(sec) = read_u32(key, "st_time") {
            return sec as i64 * 1000;
        }
        parse_date(&read_text(key, "dateAdded").unwrap_or_default()).unwrap_or(0)
    }
}

fn parse_idm(_cfg: &ScanConfig) -> Vec<DownloadItem> {
    #[cfg(windows)]
    {
        idm::parse()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/* ---------- FDM（sqlite + 自研 vmscl BLOB） ---------- */

/// `downloads` 表里我们关心的列。
struct FdmRow {
    id: i64,
    url: String,
    /// 用户选的保存目录；可能为 NULL。
    dest: String,
    /// 多数行是 NULL，所以名字主要靠 `files` 里的 details.path。
    title: String,
    created: i64,
    /// 0 = 还没完成。
    finished: i64,
    files: Vec<u8>,
    /// 失败详情（vmscl map）；只有未完成的行才有。
    last_result: Vec<u8>,
}

/// vmscl 里的一个值。容器（type 9/28）不需要展开，知道「这里是个容器」就够。
#[derive(Clone, Debug, PartialEq)]
enum VVal {
    Int(i64),
    Text(String),
    Null,
    Container,
}

/// FDM 的 `files`/`lastResult` 是自研 vmscl 容器（大端）。官方布局是
/// `u32 fileCount` 后接 `[u32 entryCount][entry...]`，entry =
/// `[u32 keyLen][UTF-16BE key][u32 type][u8 pad][value]`，但容器头部的字节
/// 实测并不统一（sections / details / map 各不一样），按结构递推会在边界上错位。
/// 所以这里改成在整块缓冲上「容忍式扫描」：认不出 key 就前进 1 字节。
/// 430 行真实库 + 20 行 lastResult 全部解出、0 报错，这个扫法才站得住。
struct Vmscl<'a> {
    buf: &'a [u8],
}

impl<'a> Vmscl<'a> {
    fn u32_at(&self, at: usize) -> Option<u32> {
        (at + 4 <= self.buf.len()).then(|| {
            u32::from_be_bytes([self.buf[at], self.buf[at + 1], self.buf[at + 2], self.buf[at + 3]])
        })
    }

    /// 大端定宽整数，`width` 取 4 或 8。
    fn int_at(&self, at: usize, width: usize) -> Option<i64> {
        if at + width > self.buf.len() {
            return None;
        }
        let mut v: i64 = 0;
        for b in &self.buf[at..at + width] {
            v = (v << 8) | (*b as i64);
        }
        // 32 位值可能是负数（实测没有，但别把符号丢了）。
        if width == 4 && v > i32::MAX as i64 {
            v -= 1i64 << 32;
        }
        Some(v)
    }

    fn utf16_be(&self, at: usize, len: usize) -> Option<String> {
        if len % 2 != 0 || at + len > self.buf.len() {
            return None;
        }
        let mut units = Vec::with_capacity(len / 2);
        let mut i = at;
        while i + 1 < at + len {
            units.push(u16::from_be_bytes([self.buf[i], self.buf[i + 1]]));
            i += 2;
        }
        Some(String::from_utf16_lossy(&units))
    }

    /// 从 `off` 试读一个 entry；不像 entry 就返回 None（调用方前进 1 字节）。
    fn entry(&self, off: usize) -> Option<(String, VVal, usize)> {
        let klen = self.u32_at(off)? as usize;
        // key 是 UTF-16BE，字节数必为偶数；真实字段名没有超过 32 个字符的。
        if klen == 0 || klen % 2 != 0 || klen > 64 {
            return None;
        }
        let ty_at = off + 4 + klen;
        let ty = self.u32_at(ty_at)?;
        let key = self.utf16_be(off + 4, klen)?;
        // 容器的头部字节会被当成 key 读出来，解出的都是控制字符，用这个筛掉。
        if !key.chars().all(|c| (c as u32) >= 32 && (c as u32) < 0x3000) {
            return None;
        }
        let val = ty_at + 5; // type 之后固定 1 字节填充
        match ty {
            2 | 3 => Some((key, VVal::Int(self.int_at(val, 4)?), val + 4)),
            4 => Some((key, VVal::Int(self.int_at(val, 8)?), val + 8)),
            10 => {
                let len = self.u32_at(val)?;
                if len == u32::MAX {
                    Some((key, VVal::Null, val + 4))
                } else {
                    let len = len as usize;
                    // 真实字符串最长几十字节，离谱的长度说明这里不是真 entry。
                    if len > 8192 {
                        return None;
                    }
                    Some((key, VVal::Text(self.utf16_be(val + 4, len)?), val + 4 + len))
                }
            }
            // 容器：跳过 pad 继续往里扫，子字段会平铺进同一个列表。
            9 | 28 => Some((key, VVal::Container, val)),
            _ => None,
        }
    }

    /// 平铺出所有认得出的 (key, value)，顺序即字节顺序。
    fn scan(&self) -> Vec<(String, VVal)> {
        let mut out = Vec::new();
        let mut off = 0usize;
        while off + 4 <= self.buf.len() {
            match self.entry(off) {
                Some((k, v, next)) if next > off => {
                    out.push((k, v));
                    off = next;
                }
                _ => off += 1,
            }
        }
        out
    }
}

/// `files` 里要用到的三样：目标文件名、总字节、已完成字节。
#[derive(Default, Clone, Debug, PartialEq)]
struct FilesInfo {
    path: String,
    total: u64,
    done: u64,
}

fn bytes_of(n: i64) -> u64 {
    n.max(0) as u64
}

fn files_info(blob: &[u8]) -> FilesInfo {
    let mut info = FilesInfo::default();
    // fileCount == 0 的行整个 blob 只有 4 字节（实测 6 行），里面没有 path/size。
    if blob.len() <= 4 {
        return info;
    }
    let flat = Vmscl { buf: blob }.scan();
    let mut top: Option<u64> = None;
    let mut section_sum = 0u64;
    let mut first: Option<u64> = None;
    for (i, (key, val)) in flat.iter().enumerate() {
        match (key.as_str(), val) {
            ("path", VVal::Text(s)) => {
                if info.path.is_empty() && !s.is_empty() {
                    info.path = file_name(s);
                }
            }
            ("size", VVal::Int(n)) => {
                let n = bytes_of(*n);
                if first.is_none() {
                    first = Some(n);
                }
                // section 的 size/done/offset 是一组三元组：紧跟其后 2 条里出现 done
                // 的就是 section 大小，否则是文件级总大小（id=1068 那个 707566379 是
                // 两个 section 之和，就是这么认出来的）。
                let in_section = flat.iter().skip(i + 1).take(2).any(|(k, _)| k.as_str() == "done");
                if in_section {
                    section_sum = section_sum.saturating_add(n);
                } else if top.is_none() {
                    top = Some(n);
                }
            }
            // done 只出现在 section 里，总和就是已下载字节。
            ("done", VVal::Int(n)) => info.done = info.done.saturating_add(bytes_of(*n)),
            _ => {}
        }
    }
    info.total = top.or(Some(section_sum).filter(|n| *n > 0)).or(first).unwrap_or(0);
    info
}

/// `lastResult` 是同一种 vmscl 容器（map），里面 category/description 是失败原因。
fn error_text(blob: &[u8]) -> Option<String> {
    if blob.len() <= 4 {
        return None;
    }
    let mut category = String::new();
    let mut description = String::new();
    let flat = Vmscl { buf: blob }.scan();
    for (key, val) in flat {
        if let VVal::Text(s) = val {
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            match key.as_str() {
                "description" => description = s.to_string(),
                "category" => category = s.to_string(),
                _ => {}
            }
        }
    }
    // 实测多数行只有 category（description 是空串），所以两级回退。
    if !description.is_empty() {
        Some(description)
    } else if !category.is_empty() {
        Some(category)
    } else {
        None
    }
}

/// FDM 的状态只能从「有没有完成时间 + 有没有错误记录」推。
/// 本机 20 行未完成的行**全部**带 lastResult，而且都是 error 类别（没有成功类），
/// 所以「未完成 + 有错 ⇒ 失败」在这台机器上等于「未完成 ⇒ 失败」。
/// 保留这个判断顺序，是因为 FDM 哪天把错清掉，这里应该自动落回下载中/等待。
fn fdm_state(finished: bool, total: u64, done: u64, err: Option<&str>, live: bool) -> DlState {
    if finished {
        // finishedTime 只有真正写完才写；size 读不出来（fileCount=0 的行）也算完成。
        return DlState::Done;
    }
    if err.is_some() {
        return DlState::Failed;
    }
    if total == 0 && done == 0 {
        return DlState::Unknown;
    }
    if done == 0 {
        return DlState::Queued;
    }
    // 库里没有速度字段，只能用「进程在不在」区分「在跑」和「上次落盘」。
    if live {
        DlState::Downloading
    } else {
        DlState::Paused
    }
}

#[cfg(windows)]
mod fdm_sql {
    use super::FdmRow;
    use std::path::Path;

    /// 只读打开。库是 rollback journal 模式，不需要写权限，也不复制整库、
    /// 不改任何 pragma —— 一切都是「读不到就这一轮没数据」。
    pub(super) fn load(path: &Path) -> Vec<FdmRow> {
        if !path.is_file() {
            return Vec::new();
        }
        let Ok(conn) = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, url, destinationPath, title, creationTime, finishedTime, files, lastResult \
             FROM downloads",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |r| {
            // 单列类型出乎意料（NULL / TEXT）时退化成默认值，别让整表读不出来。
            Ok(FdmRow {
                id: r.get::<_, i64>(0).unwrap_or(0),
                url: r.get::<_, Option<String>>(1).ok().flatten().unwrap_or_default(),
                dest: r.get::<_, Option<String>>(2).ok().flatten().unwrap_or_default(),
                title: r.get::<_, Option<String>>(3).ok().flatten().unwrap_or_default(),
                created: r.get::<_, Option<i64>>(4).ok().flatten().unwrap_or(0),
                finished: r.get::<_, Option<i64>>(5).ok().flatten().unwrap_or(0),
                files: r.get::<_, Option<Vec<u8>>>(6).ok().flatten().unwrap_or_default(),
                last_result: r.get::<_, Option<Vec<u8>>>(7).ok().flatten().unwrap_or_default(),
            })
        }) else {
            return Vec::new();
        };
        rows.filter_map(|r| r.ok()).collect()
    }
}

fn fdm_item(row: FdmRow, live: bool) -> DownloadItem {
    let info = files_info(&row.files);
    let err = error_text(&row.last_result);
    let finished = row.finished > 0;
    let state = fdm_state(finished, info.total, info.done, err.as_deref(), live);
    // 名字优先 details.path（title 大多是 NULL），再退 title，最后从 URL 末段猜。
    let mut name = info.path.clone();
    if name.is_empty() {
        name = file_name(&row.title);
    }
    if name.is_empty() {
        name = url_file_name(&row.url);
    }
    if name.is_empty() {
        name = format!("下载 #{}", row.id);
    }
    let dest = row.dest.trim();
    let path = if dest.is_empty() { PathBuf::new() } else { PathBuf::from(dest).join(&name) };
    DownloadItem {
        id: format!("{}:{}", Source::Fdm.key(), row.id),
        source: Source::Fdm,
        name,
        path,
        total: info.total,
        done: info.done,
        // 库里没有速度字段（实测），永远给 0，界面靠这个不画速度。
        speed: 0,
        state,
        created_ms: unix_to_ms(row.created),
        finished_ms: finished.then(|| unix_to_ms(row.finished)),
        detail: err,
        // live 只表示 fdm.exe 在跑：进度要等它 flush 才落盘，速度压根没有，
        // 所以界面仍然应当把这份数据当「上次记录」。
        live,
    }
}

fn parse_fdm(roots: &Roots, _cfg: &ScanConfig) -> Vec<DownloadItem> {
    #[cfg(windows)]
    {
        let live = proc_alive(Source::Fdm.exe());
        let rows = fdm_sql::load(&roots.fdm_db);
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(fdm_item(row, live));
        }
        out
    }
    #[cfg(not(windows))]
    {
        let _ = roots;
        Vec::new()
    }
}

/* ---------- Ghost Downloader（tasks.jsonl） ---------- */

/// (文件长度, 修改时间毫秒)；文件不在就给 None。
fn file_stamp(path: &std::path::Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((meta.len(), mtime))
}

fn parse_ghost(roots: &Roots, _cfg: &ScanConfig) -> Vec<DownloadItem> {
    let path = &roots.ghost_jsonl;
    // Ghost 是「整份重写」而不是追加，写一半的时候读到的就是半截列表。
    // 所以 stat → 读 → 再 stat，两次一致才采纳；不一致这轮就当没有数据。
    let Some(before) = file_stamp(path) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Some(after) = file_stamp(path) else {
        return Vec::new();
    };
    if before != after {
        return Vec::new();
    }
    ghost_items(&text, proc_alive(Source::Ghost.exe())).unwrap_or_default()
}

/// 逐行解析；任意一行坏掉就整轮放弃（返回 None），不要半个列表。
fn ghost_items(text: &str, live: bool) -> Option<Vec<DownloadItem>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        if let Some(item) = ghost_item(&value, live) {
            out.push(item);
        }
    }
    Some(out)
}

/// 进度只认 `steps[].progress`（0-100）。
/// 不能用 ΣreceivedBytes：实测有 8 个任务 recvSum = 0 却 progress = 100，字节聚合根本不自洽。
/// 多 step 取「按 step.fileSize 加权平均」：分片下载里大分片说了算；权重全为 0
/// （step 没写 fileSize，实测 zip_pack 的 WAITING 步就是这种）时退化成算术平均，
/// 否则 [100, 0] 这种已经完成的任务会被拉成 50%。
fn ghost_percent(steps: &[&serde_json::Value]) -> f64 {
    if steps.is_empty() {
        return 0.0;
    }
    let mut weighted = 0.0f64;
    let mut weight = 0.0f64;
    let mut plain = 0.0f64;
    let mut count = 0.0f64;
    for s in steps {
        let p = s.get("progress").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let w = s.get("fileSize").and_then(|x| x.as_u64()).unwrap_or(0) as f64;
        weighted += p * w;
        weight += w;
        plain += p;
        count += 1.0;
    }
    let p = if weight > 0.0 { weighted / weight } else { plain / count };
    p.clamp(0.0, 100.0)
}

/// 单文件任务没有 `files` 数组（实测 19 行里 18 行是 null），路径只能拼
/// outputFolder + name；zip_pack 任务的 `files` 用的是 `relativePath` 而不是
/// `filePath`，两种都认。
fn ghost_file_path(v: &serde_json::Value) -> String {
    let Some(files) = v.get("files").and_then(|x| x.as_array()) else {
        return String::new();
    };
    for f in files {
        for key in ["filePath", "relativePath"] {
            if let Some(s) = f.get(key).and_then(|x| x.as_str()) {
                let s = s.trim();
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    String::new()
}

fn ghost_item(v: &serde_json::Value, live: bool) -> Option<DownloadItem> {
    let sid = v.get("taskId").and_then(|x| x.as_str())?.trim();
    if sid.is_empty() {
        return None;
    }
    let steps: Vec<&serde_json::Value> =
        v.get("steps").and_then(|x| x.as_array()).map(|a| a.iter().collect()).unwrap_or_default();
    // 顶层 fileSize 最可靠；没有就按 step 的 fileSize 求和。
    let total = v.get("fileSize").and_then(|x| x.as_u64()).unwrap_or_else(|| {
        steps
            .iter()
            .filter_map(|s| s.get("fileSize").and_then(|x| x.as_u64()))
            .fold(0u64, |a, b| a.saturating_add(b))
    });
    let percent = ghost_percent(&steps);
    // done 用 progress × fileSize 反推：逐字节聚合不可信（见 ghost_percent 的说明）。
    let done = ((total as f64) * percent / 100.0).round().max(0.0) as u64;
    let speed = steps
        .iter()
        .filter_map(|s| s.get("speed").and_then(|x| x.as_u64()))
        .fold(0u64, |a, b| a.saturating_add(b));

    let raw_status = v.get("status").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let (state, status_detail) = match raw_status.as_str() {
        "COMPLETED" => (DlState::Done, None),
        "RUNNING" | "DOWNLOADING" => (DlState::Downloading, None),
        "PAUSED" => (DlState::Paused, None),
        "WAITING" => (DlState::Queued, None),
        "FAILED" => (DlState::Failed, None),
        // STOPPED 只在 exe 内部出现（状态机里比 FAILED 更早的一档），按失败算，
        // 原文留在 detail 里方便对账。
        "STOPPED" => (DlState::Failed, Some(raw_status.clone())),
        other => (DlState::Unknown, (!other.is_empty()).then(|| other.to_string())),
    };

    let output = v.get("outputFolder").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let file_path = ghost_file_path(v);
    let mut name = file_name(v.get("name").and_then(|x| x.as_str()).unwrap_or(""));
    if name.is_empty() {
        name = file_name(&file_path);
    }
    if name.is_empty() {
        name = url_file_name(v.get("url").and_then(|x| x.as_str()).unwrap_or(""));
    }
    if name.is_empty() {
        name = sid.to_string();
    }
    let path = if !file_path.is_empty() {
        PathBuf::from(file_path)
    } else if !output.is_empty() {
        PathBuf::from(output).join(&name)
    } else {
        PathBuf::new()
    };

    let created_ms = v
        .get("createdAt")
        .and_then(|x| x.as_i64())
        .or_else(|| v.get("startTime").and_then(|x| x.as_i64()))
        .map(unix_to_ms)
        .unwrap_or(0);
    let finished_ms = v
        .get("completedAt")
        .and_then(|x| x.as_i64())
        .filter(|x| *x > 0)
        .or_else(|| v.get("endTime").and_then(|x| x.as_i64()).filter(|x| *x > 0))
        .map(unix_to_ms);

    Some(DownloadItem {
        id: format!("{}:{}", Source::Ghost.key(), sid),
        source: Source::Ghost,
        name,
        path,
        total,
        done,
        speed,
        state,
        created_ms,
        finished_ms,
        detail: status_detail,
        live,
    })
}

/* ---------- 后台轮询 ---------- */

/// 每 `POLL_INTERVAL` 在后台线程扫一次，结果通过通道回传，UI 线程永不阻塞。
/// 与 `task_scan::Watcher` 同构：单飞、超时就跳过、线程死了保留上次结果。
pub struct Watcher {
    items: Vec<DownloadItem>,
    /// `inject` 塞过条目就锁住，别再让后台扫描覆盖（预览/测试用）。
    manual: bool,
    clock: PollClock,
    rx: Option<Receiver<Vec<DownloadItem>>>,
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

    pub fn items(&self) -> &[DownloadItem] {
        &self.items
    }

    pub fn get(&self, id: &str) -> Option<&DownloadItem> {
        self.items.iter().find(|it| it.id == id)
    }

    pub fn inject(&mut self, items: Vec<DownloadItem>) {
        self.items = items;
        self.manual = true;
    }

    /// 收上一次的结果；没有在跑的扫描且到点了就起一个后台线程。
    pub fn tick(&mut self, cfg: ScanConfig) {
        if self.manual {
            return;
        }
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(items) => {
                    self.items = items;
                    self.rx = None;
                }
                Err(TryRecvError::Empty) => {}
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
            .name("daozai-downloads".to_string())
            .spawn(move || {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(state: DlState, total: u64, done: u64, created_ms: i64) -> DownloadItem {
        DownloadItem {
            id: format!("t:{}", created_ms),
            source: Source::Ghost,
            name: "a.bin".to_string(),
            path: PathBuf::from("C:\\a.bin"),
            total,
            done,
            speed: 0,
            state,
            created_ms,
            finished_ms: None,
            detail: None,
            live: false,
        }
    }

    #[test]
    fn percent_never_exceeds_one() {
        assert_eq!(item(DlState::Downloading, 100, 250, 1).percent(), 1.0);
        assert_eq!(item(DlState::Downloading, 100, 25, 1).percent(), 0.25);
        assert_eq!(item(DlState::Done, 0, 0, 1).percent(), 1.0);
        assert_eq!(item(DlState::Unknown, 0, 0, 1).percent(), 0.0);
    }

    #[test]
    fn size_text_shows_partial_progress() {
        assert_eq!(item(DlState::Downloading, 0, 0, 1).size_text(), "大小未知");
        assert_eq!(item(DlState::Downloading, 2 * 1024 * 1024, 1024 * 1024, 1).size_text(), "1.00 MB / 2.00 MB");
        assert_eq!(item(DlState::Done, 2 * 1024 * 1024, 2 * 1024 * 1024, 1).size_text(), "2.00 MB");
    }

    #[test]
    fn normalize_sorts_active_first_then_newest() {
        let mut items = vec![
            item(DlState::Done, 1, 1, 300),
            item(DlState::Downloading, 1, 0, 100),
            item(DlState::Failed, 1, 0, 500),
            item(DlState::Downloading, 1, 0, 200),
        ];
        normalize(&mut items, &ScanConfig { keep_minutes: 0, ..Default::default() });
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["t:200", "t:100", "t:500", "t:300"]);
    }

    #[test]
    fn normalize_drops_items_outside_window() {
        let now = 10_000_000i64;
        // 这里的时间轴是毫秒：60 分钟前 = 3_600_000 ms（原先写成 60_000 恰好压在 keep_minutes=1 的
        // 边界上被 `>= cutoff` 留下，和注释、断言都对不上）。
        let mut items = vec![item(DlState::Done, 1, 1, now - 60 * 60_000), item(DlState::Done, 1, 1, 0)];
        normalize(&mut items, &ScanConfig { keep_minutes: 1, now_ms: now, ..Default::default() });
        // 60 分钟前的被丢掉，创建时间未知的留着。
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "t:0");
    }

    /* ---------- 真实数据样本 ---------- */

    /// 本机 FDM 库 `downloads.id = 1599` 的 `files` BLOB 原样十六进制（747 字节）。
    /// 解出来的期望值就是 research 文档里记的那条。
    const FDM_FILES_1599_HEX: &str = concat!(
        "000000010000000a0000000800730069007a0065000000040000000000017f926d00000014006f0072006900670069006e00",
        "5400790070006500000002000000000100000010007000720069006f007200690074007900000002000000000a0000001000",
        "730065006300740069006f006e00730000000900000000010000001c00000000030000000800730069007a00650000000400",
        "00000000017f926d0000000c006f0066006600730065007400000004000000000000000000000000080064006f006e006500",
        "0000040000000000017f926d0000000e00640065007400610069006c00730000001c00000000090000000600660070007300",
        "0000020000000000000000080074007900700065000000020000000000000000160063006f006e00740065006e0074005400",
        "79007000650000000a0000000030006100700070006c00690063006100740069006f006e002f006f0063007400650074002d",
        "00730074007200650061006d0000000c0068006500690067006800740000000200000000000000000c0066006f0072006d00",
        "6100740000000a00ffffffff0000000e006200690074007200610074006500000002000000000000000010006c0061006e00",
        "6700750061006700650000000a00ffffffff0000000a00770069006400740068000000020000000000000000100064007500",
        "72006100740069006f006e00000002000000000000000018006c006100730074004d006f0064006900660069006500640000",
        "000400000001a1d5d20fa00000000800700061007400680000000a000000004c0073006e006f0077002d00730068006f0074",
        "002d0031002e0035002e0037002d00770069006e0064006f00770073002d007800360034002d006f006e006c0069006e0065",
        "002e0065007800650000000e0074006d007000500061007400680000000a00ffffffff0000000a0066006c00610067007300",
        "00000300000000030000001600770072006900740065004f0066006600730065007400000004000000000000000000",
    );

    /// 本机 Ghost `tasks.jsonl` 里最短的一行（原样，只把 `headers` 键删掉以缩短长度）。
    const GHOST_LINE: &str = r#"{"name": "1040g2jo327cimsjsn8005o661cvgbr4t4u5he1g.webp", "url": "https://sns-avatar-qc.xhscdn.com/avatar/1040g2jo327cimsjsn8005o661cvgbr4t4u5he1g?imageView2/2/w/540/format/webp|imageMogr2/strip2", "packId": "http", "taskId": "tsk_3ad745f75028446cba776d933b7e011e", "status": "COMPLETED", "steps": [{"stepIndex": 1, "fileIndex": null, "stepId": "stp_2278f9d2a19540e39123bd1134aec3a3", "status": "COMPLETED", "progress": 100, "receivedBytes": 0, "speed": 0, "url": "https://sns-avatar-qc.xhscdn.com/avatar/1040g2jo327cimsjsn8005o661cvgbr4t4u5he1g?imageView2/2/w/540/format/webp|imageMogr2/strip2", "fileSize": 16032, "clientProfile": "", "userAgent": "", "subworkerCount": 16, "canUseRangeRequests": true, "httpByteOffset": 0, "lastModified": "", "filePath": "", "isAccelerated": false, "type": "http_pack.task.HttpTaskStep"}], "createdAt": 1794715341, "completedAt": 1794715342, "outputFolder": "D:\\下载", "fileSize": 16032, "files": null, "category": null, "shouldSeed": true, "checksums": {}, "type": "http_pack.task.HttpTask"}"#;

    fn from_hex(s: &str) -> Vec<u8> {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() / 2);
        let mut i = 0;
        while i + 1 < bytes.len() {
            let hi = (bytes[i] as char).to_digit(16);
            let lo = (bytes[i + 1] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
                _ => break,
            }
            i += 2;
        }
        out
    }

    #[test]
    fn idm_u64_decodes_raw_reg_none_bytes() {
        // 本机 id=548 的 FileSize 原始字节：47504048
        assert_eq!(u64_le(&[0xb0, 0xda, 0xd4, 0x02, 0x00, 0x00, 0x00, 0x00]), Some(47_504_048));
        // 老条目只写 4 字节也要能用
        assert_eq!(u64_le(&[0x5f, 0x6e, 0x00, 0x00]), Some(28_255));
        assert_eq!(u64_le(&[]), None);
    }

    #[test]
    fn idm_date_string_parses_as_local_time() {
        let ms = parse_date("Apr 12 15:15:47 2025").expect("IDM 的 dateAdded 应该能解析");
        let got = chrono::DateTime::from_timestamp_millis(ms)
            .expect("合法时间戳")
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert_eq!(got, "2025-04-12 15:15:47");
        // 日期只占一格宽度（IDM 对 1 位数日期是空格补齐）也要能读
        assert!(parse_date("Apr  2 09:05:01 2025").is_some());
        assert_eq!(parse_date("2025-04-12T15:15:47"), None);
        assert_eq!(parse_date(""), None);
    }

    #[test]
    fn vmscl_reads_real_fdm_file_blob() {
        let blob = from_hex(FDM_FILES_1599_HEX);
        assert_eq!(blob.len(), 747, "样本长度不对，十六进制抄错了");
        let info = files_info(&blob);
        assert_eq!(info.path, "snow-shot-1.5.7-windows-x64-online.exe");
        assert_eq!(info.total, 25_137_773);
        assert_eq!(info.done, 25_137_773);
    }

    #[test]
    fn vmscl_tolerates_file_blob_without_entries() {
        // fileCount = 0 的行只有 4 个字节（实测 6 行），不能崩也不该编出 size
        let blob = from_hex("00000000");
        assert_eq!(files_info(&blob), FilesInfo::default());
        assert_eq!(error_text(&blob), None);
    }

    #[test]
    fn fdm_state_follows_finished_then_error() {
        assert_eq!(fdm_state(true, 100, 100, None, false), DlState::Done);
        assert_eq!(fdm_state(true, 0, 0, None, false), DlState::Done);
        assert_eq!(fdm_state(false, 100, 40, Some("连接已关闭"), false), DlState::Failed);
        assert_eq!(fdm_state(false, 0, 0, None, false), DlState::Unknown);
        assert_eq!(fdm_state(false, 100, 0, None, true), DlState::Queued);
        assert_eq!(fdm_state(false, 100, 40, None, false), DlState::Paused);
        assert_eq!(fdm_state(false, 100, 40, None, true), DlState::Downloading);
    }

    #[test]
    fn ghost_line_parses_progress_from_steps() {
        let items = ghost_items(GHOST_LINE, false).expect("真实样本应该能解析");
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.id, "ghost:tsk_3ad745f75028446cba776d933b7e011e");
        assert_eq!(it.name, "1040g2jo327cimsjsn8005o661cvgbr4t4u5he1g.webp");
        assert_eq!(it.total, 16_032);
        // receivedBytes 是 0，但 progress = 100：所以 done 只能从 progress 推。
        assert_eq!(it.done, 16_032);
        assert_eq!(it.percent(), 1.0);
        assert_eq!(it.state, DlState::Done);
        // files 是 null，路径只能拼 outputFolder + name
        assert_eq!(it.path, PathBuf::from("D:\\下载").join(it.name.as_str()));
        // 秒 → 毫秒
        assert_eq!(it.created_ms, 1_794_715_341_000);
        assert_eq!(it.finished_ms, Some(1_794_715_342_000));
    }

    #[test]
    fn ghost_progress_weights_steps_by_file_size() {
        // 复刻实测的 tsk_a7fcf55f：第 1 个 step 完成，第 2 个是 WAITING 且没写 fileSize。
        // 算术平均会得到 50%，加权才对（这个任务是 COMPLETED）。
        let line = r#"{"taskId":"tsk_x","name":"pack.zip","status":"COMPLETED","steps":[{"progress":100,"speed":0,"fileSize":1000},{"progress":0,"speed":0}],"createdAt":1794655983,"completedAt":1794655990,"outputFolder":"D:\\下载","fileSize":1000,"files":null}"#;
        let items = ghost_items(line, false).expect("解析");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].percent(), 1.0);
        assert_eq!(items[0].path, PathBuf::from("D:\\下载\\pack.zip"));
    }

    #[test]
    fn ghost_status_maps_and_keeps_unknown_text() {
        let stopped = r#"{"taskId":"tsk_y","name":"a.bin","status":"STOPPED","steps":[{"progress":50,"speed":0,"fileSize":10}],"createdAt":1794655983,"fileSize":10,"files":null,"outputFolder":"C:\\t"}"#;
        let items = ghost_items(stopped, false).expect("解析");
        assert_eq!(items[0].state, DlState::Failed);
        assert_eq!(items[0].detail.as_deref(), Some("STOPPED"));

        // 不认识的取值 → Unknown，原文塞 detail
        let odd = r#"{"taskId":"tsk_z","name":"b.bin","status":"WHAT","steps":[],"createdAt":1,"fileSize":0,"files":null}"#;
        let items = ghost_items(odd, false).expect("解析");
        assert_eq!(items[0].state, DlState::Unknown);
        assert_eq!(items[0].detail.as_deref(), Some("WHAT"));

        // 毫秒时间戳也要认（> 1e11）
        assert_eq!(unix_to_ms(1_794_655_983), 1_794_655_983_000);
        assert_eq!(unix_to_ms(1_794_655_983_000), 1_794_655_983_000);
        assert_eq!(unix_to_ms(0), 0);
    }

    #[test]
    fn ghost_half_written_file_is_dropped_whole() {
        // 任何一行坏掉都不能交出半截列表
        let text = format!("{}\n{{\"taskId\": \"tsk_broken\", ", GHOST_LINE);
        assert!(ghost_items(&text, false).is_none());
    }

    #[test]
    fn missing_sources_yield_empty() {
        let roots = Roots {
            fdm_db: PathBuf::from("C:\\__no_such_dir__\\db.sqlite"),
            ghost_jsonl: PathBuf::from("C:\\__no_such_dir__\\tasks.jsonl"),
        };
        assert!(parse_fdm(&roots, &ScanConfig::default()).is_empty());
        assert!(parse_ghost(&roots, &ScanConfig::default()).is_empty());
    }

    #[test]
    fn url_fallback_name_needs_an_extension() {
        assert_eq!(url_file_name("https://a.b/c/setup.exe?x=1"), "setup.exe");
        assert_eq!(url_file_name("https://a.b/"), "");
        assert_eq!(file_name("dir\\file.rar?token=1"), "file.rar");
        assert_eq!(file_name("  "), "");
    }

    /* ---------- 真机（本机真实数据，--ignored 单跑） ---------- */

    #[test]
    #[ignore]
    #[cfg(windows)]
    fn live_idm_finds_47504048() {
        let items = parse_idm(&ScanConfig::default());
        println!("IDM 命中 {} 条", items.len());
        for it in items.iter().take(5) {
            println!("  {} | {} | {} | {}", it.id, it.name, it.size_text(), it.subtitle(true));
        }
        assert!(items.len() >= 340, "本机 IDM 应有 345 个数字 id 任务，实际 {}", items.len());
        assert!(items.iter().any(|it| it.total == 47_504_048), "应有 FileSize = 47504048 的任务");
        let it = items.iter().find(|it| it.id == "idm:548").expect("应有 idm:548");
        assert_eq!(it.done, 47_504_048);
        assert_eq!(it.state, DlState::Done);
        // 全局保存目录取到了才会拼出完整路径
        println!("  548 path = {}", it.path.display());
        assert!(it.path.to_string_lossy().ends_with("fdm_x64_setup.exe"), "路径应拼上文件名");
    }

    #[test]
    #[ignore]
    fn live_fdm_finds_25137773() {
        let roots = Roots::real();
        let items = parse_fdm(&roots, &ScanConfig::default());
        println!("FDM 命中 {} 条（{}）", items.len(), roots.fdm_db.display());
        assert!(items.iter().any(|it| it.total == 25_137_773), "应有 size = 25137773 的任务");
        let it = items.iter().find(|it| it.id == "fdm:1599").expect("应有 fdm:1599");
        assert_eq!(it.name, "snow-shot-1.5.7-windows-x64-online.exe");
        assert_eq!(it.total, 25_137_773);
        assert_eq!(it.done, 25_137_773);
        assert_eq!(it.state, DlState::Done);
        println!("  1599 = {} / {}", it.done, it.total);
        for it in items.iter().filter(|it| it.state == DlState::Failed).take(3) {
            println!("  {} | {} | {} | {:?}", it.id, it.name, it.size_text(), it.detail);
        }
    }

    #[test]
    #[ignore]
    fn live_ghost_reads_every_task() {
        let roots = Roots::real();
        let items = parse_ghost(&roots, &ScanConfig::default());
        println!("Ghost 命中 {} 条（{}）", items.len(), roots.ghost_jsonl.display());
        for it in items.iter().take(4) {
            println!("  {} | {} | {} | {}", it.id, it.name, it.size_text(), it.subtitle(true));
        }
        // 条数只随真实使用变化，不当成断言：这里要证的是「能读全、字段对」。
        assert!(items.len() >= 19, "本机 tasks.jsonl 至少 19 条，实读 {}", items.len());
        assert!(
            items.iter().filter(|it| it.state == DlState::Done).all(|it| it.total > 0),
            "每条完成的都应拿到 fileSize"
        );
        assert!(items.iter().all(|it| !it.name.is_empty()), "每条都要有名字");
    }

    /// 真机快照：把三个来源合并后「岛上会显示什么」打出来（真下载实测时用）。
    #[test]
    #[ignore]
    fn live_dump_what_the_island_would_show() {
        let roots = Roots::real();
        let cfg = ScanConfig::default();
        let items = scan(&roots, &cfg);
        println!("=== 合并 {} 条 ===", items.len());
        for it in &items {
            println!(
                "{:>9} | {:<6} | {:<8} | p={:>5.1}% | {:<14} | {} | {}",
                it.id,
                it.source.label(),
                it.state.label(),
                it.percent() * 100.0,
                it.speed_text(),
                it.size_text(),
                it.name
            );
        }
        let now = now_ms();
        if let Some(pick) = crate::download_ui::pick_island_download(&items, now) {
            println!("岛上会报：{} / {} / {}", pick.id, pick.name, pick.subtitle(true));
            if let Some(badge) = crate::download_ui::badge_of(pick) {
                println!("角标：rate={:.2} MB/s progress={:?}", badge.rate, badge.progress);
            }
        } else {
            println!("岛上没有要报的下载（没有进行中的，也没有 6 秒内收场的）");
        }
    }
}
