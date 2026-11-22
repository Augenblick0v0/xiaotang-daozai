//! 字幕识别记录：按 id 合并实时字幕的定稿行，按容量上限保留最近若干条，并导出 TXT/SRT。
//!
//! 为什么不塞进 `caption`：识别侧只关心「此刻屏幕上那几行」，是易失状态；
//! 历史要跨会话留下、要能整段导出，生命周期完全不同。这里只做纯粹的
//! 存储 / 裁剪 / 导出，不碰线程、不碰 UI，调用方在主线程持 `&mut` 即可。
//!
//! 落盘是 JSONL 追加日志：每条新记录 / 每次同 id 更新追加一行，`load` 时按 id
//! 重放（后写的覆盖先写的）。只有裁剪、`clear` 这类「删除」表达不了的变更才整份
//! 重写——否则旧行会永远留在文件里。

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一条识别记录。
///
/// `id` 由识别器给出：同一句的中间结果和最终结果 id 相同，译文晚到也不换 id，
/// 所以历史按 id 合并，而不是按文本去重（同一句话可能被识别两遍）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub text: String,
    pub translation: String,
    pub lang: String,
    pub start: f32,
    pub end: f32,
}

/// 字幕历史（单线程使用；容量上限按序列化后的字节数计）。
pub struct History {
    path: PathBuf,
    cap_bytes: usize,
    entries: Vec<Entry>,
    /// 还没追加进文件的条目：record 新增或更新时入队，flush 一次性写出。
    pending: Vec<Entry>,
    /// 有删除类变更（裁剪 / clear）时必须整文件重写，追加日志删不掉旧行。
    rewrite: bool,
}

impl History {
    /// 从磁盘加载（文件不存在返回空）。`cap_bytes` 是容量上限。
    ///
    /// 坏行直接跳过：进程被杀时最后一行可能只写了一半，不能因为一行脏数据
    /// 就把整份历史丢掉。
    pub fn load(path: PathBuf, cap_bytes: usize) -> Self {
        let mut history = Self {
            path,
            cap_bytes,
            entries: Vec::new(),
            pending: Vec::new(),
            rewrite: false,
        };
        let Ok(raw) = fs::read(&history.path) else {
            // 不存在（或暂时读不了）都从空历史开始；首次 flush 会建目录建文件。
            return history;
        };
        // 用 lossy 而不是 read_to_string：坏字节只影响它所在的那一行，其余记录还能救。
        let text = String::from_utf8_lossy(&raw);
        let mut seen: HashMap<String, usize> = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Entry>(line) else {
                continue;
            };
            let id = entry.id.clone();
            match seen.get(&id).copied() {
                // 追加日志里同 id 可能有多行：后面的更新覆盖前面的，顺序仍按首次出现。
                Some(index) => history.entries[index] = entry,
                None => {
                    seen.insert(id, history.entries.len());
                    history.entries.push(entry);
                }
            }
        }
        // 上限可能在两次运行之间被调小；加载时就裁掉，别让内存先超限。
        if history.trim() {
            history.rewrite = true;
        }
        history
    }

    /// 按 id 合并：已存在则更新文本/译文/时间，不存在则追加；随后按 `cap_bytes` 从最旧裁剪。
    pub fn record(&mut self, entry: Entry) {
        match self.entries.iter().position(|item| item.id == entry.id) {
            Some(index) => {
                let slot = &mut self.entries[index];
                // 语言是这句话本身的属性，不随后到的译文改；文本/译文/时间才会被后续快照修正。
                slot.text = entry.text;
                slot.translation = entry.translation;
                slot.start = entry.start;
                slot.end = entry.end;
                self.pending.push(slot.clone());
            }
            None => {
                self.entries.push(entry.clone());
                self.pending.push(entry);
            }
        }
        if self.trim() {
            self.pending.clear();
            self.rewrite = true;
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending.clear();
        // 删除必须靠重写文件生效；只清内存的话下次 load 会把旧行读回来。
        self.rewrite = true;
    }

    pub fn set_cap_bytes(&mut self, cap_bytes: usize) {
        self.cap_bytes = cap_bytes;
        if self.trim() {
            self.pending.clear();
            self.rewrite = true;
        }
    }

    /// 追加落盘（内部用 BufWriter，遇到目录不存在要创建父目录）。
    pub fn flush(&mut self) -> std::io::Result<()> {
        if !self.rewrite && self.pending.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let file = if self.rewrite {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?
        } else {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?
        };
        let mut writer = BufWriter::new(file);
        if self.rewrite {
            for entry in &self.entries {
                write_line(&mut writer, entry)?;
            }
        } else {
            for entry in &self.pending {
                write_line(&mut writer, entry)?;
            }
        }
        writer.flush()?;
        self.pending.clear();
        self.rewrite = false;
        Ok(())
    }

    /// `flush` 的别名：调用方历史上用 `save` 命名，保留它能少改一处调用点。
    pub fn save(&mut self) -> std::io::Result<()> {
        self.flush()
    }

    pub fn export_txt(&self, out: &Path) -> std::io::Result<()> {
        let mut writer = BufWriter::new(create_export(out)?);
        for entry in &self.entries {
            writeln!(writer, "[{}] {}", fmt_clock(entry.start), entry.text)?;
            if !entry.translation.is_empty() {
                // 12 个空格正好让译文和上一行的原文左对齐（"[HH:MM:SS] " 的宽度）。
                writeln!(writer, "            {}", entry.translation)?;
            }
        }
        writer.flush()
    }

    pub fn export_srt(&self, out: &Path) -> std::io::Result<()> {
        let mut writer = BufWriter::new(create_export(out)?);
        for (index, entry) in self.entries.iter().enumerate() {
            writeln!(writer, "{}", index + 1)?;
            writeln!(
                writer,
                "{} --> {}",
                fmt_srt(entry.start),
                fmt_srt(cue_end(entry))
            )?;
            writeln!(writer, "{}", entry.text)?;
            if !entry.translation.is_empty() {
                writeln!(writer, "{}", entry.translation)?;
            }
            // 空行是 SRT 的 cue 分隔符，最后一条也要留，播放器才认。
            writeln!(writer)?;
        }
        writer.flush()
    }

    /// 估算当前占用字节数（序列化后长度，含行尾换行——按磁盘实际占用算才准）。
    pub fn bytes(&self) -> usize {
        self.entries.iter().map(entry_bytes).sum()
    }

    /// 从最旧开始删，直到 `bytes() <= cap_bytes`；至少留 1 条（除非为空）。
    /// 返回是否真的删了东西（删了就必须整份重写）。
    fn trim(&mut self) -> bool {
        let mut total: usize = self.entries.iter().map(entry_bytes).sum();
        let mut drop_count = 0;
        while drop_count + 1 < self.entries.len() && total > self.cap_bytes {
            total -= entry_bytes(&self.entries[drop_count]);
            drop_count += 1;
        }
        if drop_count == 0 {
            return false;
        }
        self.entries.drain(..drop_count);
        true
    }
}

/// 一条记录落盘后的字节数（JSON 正文 + 换行）。
fn entry_bytes(entry: &Entry) -> usize {
    serde_json::to_string(entry)
        .map(|line| line.len() + 1)
        .unwrap_or(0)
}

fn write_line(writer: &mut impl Write, entry: &Entry) -> std::io::Result<()> {
    let line = serde_json::to_string(entry)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")
}

fn create_export(out: &Path) -> std::io::Result<File> {
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    File::create(out)
}

/// 把秒数归一到非负有限值：NaN / 负数（识别器偶尔会给）都按 0 处理。
fn seconds_of(time: f32) -> f64 {
    if time.is_finite() && time > 0.0 {
        time as f64
    } else {
        0.0
    }
}

/// TXT 用的 `HH:MM:SS`（向下取整到秒）。
fn fmt_clock(time: f32) -> String {
    let total = seconds_of(time) as u64;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

/// SRT 用的 `HH:MM:SS,mmm`（四舍五入到毫秒，避免 1.9999 显示成 1,999 差一毫秒）。
fn fmt_srt(time: f32) -> String {
    let total_ms = (seconds_of(time) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        total_ms / 3_600_000,
        (total_ms / 60_000) % 60,
        (total_ms / 1000) % 60,
        total_ms % 1000
    )
}

/// 结束时间：`end <= start`（或 end 非法）时按 `start + 1.0`，保证 SRT 每条都有正时长。
fn cue_end(entry: &Entry) -> f32 {
    if entry.end.is_finite() && entry.end > entry.start {
        entry.end
    } else {
        entry.start + 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    /// 每个测试用独立临时文件；Drop 时删除，断言失败也不留垃圾。
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str, extension: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let serial = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "daozai-caption-history-{tag}-{}-{nanos}-{serial}.{extension}",
                std::process::id()
            );
            Self(std::env::temp_dir().join(name))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn entry(id: &str, text: &str) -> Entry {
        Entry {
            id: id.into(),
            text: text.into(),
            ..Entry::default()
        }
    }

    #[test]
    fn record_merges_same_id_and_keeps_order() {
        let file = TempFile::new("merge", "jsonl");
        let mut history = History::load(file.path().to_path_buf(), usize::MAX);
        let mut first = entry("a", "first");
        first.lang = "en".into();
        history.record(first);
        history.record(entry("b", "second"));

        let mut corrected = entry("a", "first-fixed");
        corrected.translation = "第一".into();
        corrected.lang = "zh".into();
        corrected.start = 1.0;
        corrected.end = 2.0;
        history.record(corrected);

        assert_eq!(history.len(), 2);
        assert_eq!(history.entries()[0].id, "a");
        assert_eq!(history.entries()[0].text, "first-fixed");
        assert_eq!(history.entries()[0].translation, "第一");
        // 语言属于这句话本身，不随后到的快照改。
        assert_eq!(history.entries()[0].lang, "en");
        assert!((history.entries()[0].end - 2.0).abs() < f32::EPSILON);
        assert_eq!(history.entries()[1].id, "b");

        // 空文本、空译文也要能记下来（静音段 / 还在等译文）。
        history.record(Entry {
            id: "c".into(),
            ..Entry::default()
        });
        assert_eq!(history.len(), 3);
        assert!(history.entries()[2].text.is_empty());
    }

    #[test]
    fn record_trims_oldest_until_under_cap() {
        let file = TempFile::new("trim", "jsonl");
        let mut history = History::load(file.path().to_path_buf(), usize::MAX);
        for index in 0..6 {
            history.record(entry(&format!("id{index}"), &"文".repeat(50)));
        }

        // 只留下最后两条所需的字节数。
        let cap: usize = history.entries()[4..].iter().map(entry_bytes).sum();
        history.set_cap_bytes(cap);

        assert_eq!(history.len(), 2);
        assert_eq!(history.entries()[0].id, "id4");
        assert_eq!(history.entries()[1].id, "id5");
        assert!(history.bytes() <= cap);

        // 一条就超限时也保留这一条，免得刚说完的字被清空。
        history.set_cap_bytes(0);
        assert_eq!(history.len(), 1);
        assert_eq!(history.entries()[0].id, "id5");
    }

    #[test]
    fn export_txt_formats_time_and_indents_translation() {
        let source = TempFile::new("txt-source", "jsonl");
        let out = TempFile::new("txt-export", "txt");
        let mut history = History::load(source.path().to_path_buf(), usize::MAX);
        history.record(Entry {
            id: "1".into(),
            text: "hello".into(),
            translation: "你好".into(),
            start: 1.0,
            end: 2.5,
            ..Entry::default()
        });
        history.record(Entry {
            id: "2".into(),
            text: "world".into(),
            start: 62.0,
            end: 62.0,
            ..Entry::default()
        });

        history.export_txt(out.path()).unwrap();

        let text = fs::read_to_string(out.path()).unwrap();
        let expected = format!(
            "[00:00:01] hello\n{}你好\n[00:01:02] world\n",
            " ".repeat(12)
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn export_srt_numbers_cues_and_formats_time() {
        let source = TempFile::new("srt-source", "jsonl");
        let out = TempFile::new("srt-export", "srt");
        let mut history = History::load(source.path().to_path_buf(), usize::MAX);
        history.record(Entry {
            id: "1".into(),
            text: "one".into(),
            translation: "一".into(),
            start: 1.0,
            // end <= start：必须补成 start + 1.0，不能输出零时长 cue。
            end: 1.0,
            ..Entry::default()
        });
        history.record(Entry {
            id: "2".into(),
            text: "two".into(),
            start: 3661.234,
            end: 3662.5,
            ..Entry::default()
        });

        history.export_srt(out.path()).unwrap();

        let text = fs::read_to_string(out.path()).unwrap();
        assert_eq!(
            text,
            "1\n00:00:01,000 --> 00:00:02,000\none\n一\n\n\
             2\n01:01:01,234 --> 01:01:02,500\ntwo\n\n"
        );
    }

    #[test]
    fn load_skips_bad_lines() {
        let file = TempFile::new("bad-lines", "jsonl");
        let good_a = serde_json::to_string(&entry("a", "alpha")).unwrap();
        let good_b = serde_json::to_string(&entry("b", "beta")).unwrap();
        fs::write(
            file.path(),
            format!("{good_a}\nnot json at all\n\n{{\"id\":\"half\n{good_b}\n"),
        )
        .unwrap();

        let history = History::load(file.path().to_path_buf(), usize::MAX);
        assert_eq!(history.len(), 2);
        assert_eq!(history.entries()[0].id, "a");
        assert_eq!(history.entries()[0].text, "alpha");
        assert_eq!(history.entries()[1].text, "beta");
    }

    #[test]
    fn flush_appends_and_reload_dedupes_by_id() {
        let file = TempFile::new("flush", "jsonl");
        let path = file.path().to_path_buf();
        let mut history = History::load(path.clone(), 4096);
        history.record(entry("a", "one"));
        history.flush().unwrap();
        assert!(path.exists());

        history.record(Entry {
            id: "a".into(),
            text: "one-fixed".into(),
            translation: "一".into(),
            ..Entry::default()
        });
        history.record(entry("b", "two"));
        history.flush().unwrap();

        // 追加日志会有同 id 的旧行，load 必须按 id 重放出最新状态。
        let reloaded = History::load(path.clone(), 4096);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.entries()[0].id, "a");
        assert_eq!(reloaded.entries()[0].text, "one-fixed");
        assert_eq!(reloaded.entries()[0].translation, "一");
        assert_eq!(reloaded.entries()[1].text, "two");

        // clear 之后 flush 必须把文件也截断，否则下次启动旧记录会“复活”。
        let mut cleared = History::load(path.clone(), 4096);
        cleared.clear();
        cleared.flush().unwrap();
        assert!(History::load(path, 4096).is_empty());
    }

    #[test]
    fn flush_creates_missing_parent_directories() {
        let root = std::env::temp_dir().join(format!(
            "daozai-caption-history-dirs-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let path = root.join("nested").join("history.jsonl");
        let mut history = History::load(path.clone(), usize::MAX);
        history.record(entry("a", "one"));
        history.flush().unwrap();
        assert!(path.exists());

        let reloaded = History::load(path, usize::MAX);
        assert_eq!(reloaded.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }
}
