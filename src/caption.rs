//! 实时字幕：把系统声音（或麦克风）送进本机侧车，把回来的快照归一化成岛上要画的状态。
//!
//! 分工：
//! - 采音在 `caption_audio`（WASAPI 环回/麦克风，重采样到 16 kHz 单声道）；
//! - 识别与翻译在 Python 侧车（WhisperLiveKit + NLLB），见 `tools/wlk_bridge`；
//! - 这里只做「搬运 + 状态」：一个后台线程持有 WS 连接，UI 每帧读一份快照。
//!
//! 之所以用「快照」而不是事件流：侧车每 150~250 ms 就发一份全量（行数很少），
//! UI 直接替换即可，不用维护增量合并的坑。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// 字幕现在处于什么状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Status {
    /// 没开。
    #[default]
    Off,
    /// 正在准备（启动侧车 / 等模型 / 连 WS）。`stage` 里有一句人话。
    Starting,
    /// 在听，能出字。
    Running,
    /// 出错了，`error` 里是原因。
    Failed,
}

/// 一行字幕。同一句在多份快照里 `id` 不变，译文晚到时按 id 合并。
#[derive(Clone, Debug, Default)]
pub struct Line {
    pub id: String,
    pub text: String,
    pub translation: String,
    /// 这句的原文语言（`en` / `zh` / `ja`…），来自识别器的 detected_language。
    pub lang: String,
    pub start: f32,
    pub end: f32,
    /// 已经定稿（后面不会再改字）。
    pub done: bool,
}

/// UI 侧读到的不可变快照。
#[derive(Clone, Debug, Default)]
pub struct View {
    pub status: Status,
    /// 启动阶段的进度文案（如「正在加载识别模型（首次约 1.5 GB）」）。
    pub stage: String,
    /// 最近一句的原文语言。
    pub lang: String,
    pub lang_prob: f32,
    pub lines: Vec<Line>,
    /// 还没定稿的那半句（侧车的 buffer_transcription）。
    pub partial: String,
    pub error: Option<String>,
    pub translate: bool,
    pub target: String,
    /// 侧车自报的 device（cuda / cpu），给页面显示用。
    pub device: String,
    pub model: String,
    /// 最近一次收到字幕快照的时刻（用于「多久没动静」）。
    pub last_at: Option<Instant>,
}

/// 一次启动需要的参数（从 `Settings` 拷过来，线程里不再碰 UI 状态）。
#[derive(Clone, Debug)]
pub struct StartConfig {
    pub source: crate::caption_audio::CaptureSource,
    pub translate: bool,
    pub target: String,
    pub python: String,
    pub model: String,
    pub force_cpu: bool,
    pub beam: u8,
}

enum Cmd {
    Stop,
    Translate(bool, String),
}

struct Shared {
    view: View,
}

pub struct Captions {
    shared: Arc<Mutex<Shared>>,
    cmd: Option<mpsc::Sender<Cmd>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Default for Captions {
    fn default() -> Self {
        Self::new()
    }
}

impl Captions {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared { view: View::default() })),
            cmd: None,
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    /// 现在是不是有线程在跑（含「正在启动」）。
    pub fn active(&self) -> bool {
        self.thread.is_some()
    }

    /// 读一份快照。锁竞争只发生在毫秒级，UI 每帧调用是可接受的。
    pub fn view(&self) -> View {
        match self.shared.lock() {
            Ok(shared) => shared.view.clone(),
            Err(poisoned) => poisoned.into_inner().view.clone(),
        }
    }

    fn set(&self, edit: impl FnOnce(&mut View)) {
        if let Ok(mut shared) = self.shared.lock() {
            edit(&mut shared.view);
        }
    }

    /// 启动。已在跑就先停（换音源/换语言都得重连）。
    pub fn start(&mut self, cfg: StartConfig) {
        self.stop();
        let shared = self.shared.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let (tx, rx) = mpsc::channel::<Cmd>();
        {
            let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
            shared.view = View {
                status: Status::Starting,
                stage: "正在准备识别引擎…".into(),
                translate: cfg.translate,
                target: cfg.target.clone(),
                model: cfg.model.clone(),
                ..Default::default()
            };
        }
        let thread = std::thread::Builder::new()
            .name("caption".into())
            .spawn(move || {
                run(shared, stop_thread, rx, cfg);
            })
            .ok();
        self.cmd = Some(tx);
        self.stop = stop;
        self.thread = thread;
    }

    /// 停：置标志 + 断开（线程会在几百毫秒内退出）。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(tx) = self.cmd.take() {
            let _ = tx.send(Cmd::Stop);
        }
        if let Some(thread) = self.thread.take() {
            // 侧车启动可能要几秒（等模型），这里给它 6 秒收尾，超时就不等了——
            // 线程自己有 stop 标志，会在下一次检查时退出。
            let deadline = Instant::now() + Duration::from_secs(6);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.set(|view| {
            view.status = Status::Off;
            view.stage.clear();
            view.partial.clear();
        });
    }

    /// 热切换翻译：不断连接，只换目标语言（侧车按 query 里带的 target 走，
    /// 所以真要换语言还是得重连——这里用「记下来 + 由调用方 restart」的策略）。
    pub fn set_translation(&mut self, on: bool, target: &str) {
        self.set(|view| {
            view.translate = on;
            view.target = target.to_string();
        });
        if let Some(tx) = &self.cmd {
            let _ = tx.send(Cmd::Translate(on, target.to_string()));
        }
    }
}

impl Drop for Captions {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(tx) = self.cmd.take() {
            let _ = tx.send(Cmd::Stop);
        }
        // 不 join：进程退出时后台线程跟着走，避免关岛时被卡住。
    }
}

/// 后台线程：起侧车 → 连 WS → 一边送音频一边收快照，直到 stop。
fn run(shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>, rx: mpsc::Receiver<Cmd>, cfg: StartConfig) {
    let mut cfg = cfg;
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let stage = |text: &str| {
            publish(&shared, |view| {
                if view.status == Status::Starting {
                    view.stage = text.to_string();
                }
            });
        };
        match crate::caption_runtime::start(
            &cfg.python,
            &cfg.model,
            cfg.force_cpu,
            cfg.beam,
            cfg.translate,
            &stage,
            &stop,
        ) {
            Ok(sidecar) => {
                let result = session(&shared, &stop, &rx, &cfg, &sidecar);
                drop(sidecar);
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                match result {
                    SessionEnd::Stopped => return,
                    SessionEnd::Retry(reason) => {
                        publish(&shared, |view| {
                            view.status = Status::Starting;
                            view.stage = format!("连接断了（{reason}），2 秒后重连…");
                        });
                        if sleep_while(&stop, Duration::from_secs(2)) {
                            return;
                        }
                    }
                }
            }
            Err(error) => {
                publish(&shared, |view| {
                    view.status = Status::Failed;
                    view.error = Some(error);
                    view.stage = "识别引擎没起来".into();
                });
                return;
            }
        }
        // 命令里可能要求换翻译语言：重连时带上新值。
        if let Ok(Cmd::Translate(on, target)) = rx.try_recv() {
            cfg.translate = on;
            cfg.target = target;
        }
    }
}

enum SessionEnd {
    Stopped,
    Retry(String),
}

fn session(
    shared: &Arc<Mutex<Shared>>,
    stop: &Arc<AtomicBool>,
    rx: &mpsc::Receiver<Cmd>,
    cfg: &StartConfig,
    sidecar: &crate::caption_runtime::Sidecar,
) -> SessionEnd {
    let mut ws_cfg = tungstenite::protocol::WebSocketConfig::default();
    ws_cfg.max_message_size = Some(4 << 20);
    let url = format!(
        "ws://127.0.0.1:{}/asr?language=auto&translate={}&target_language={}",
        sidecar.port,
        if cfg.translate { 1 } else { 0 },
        urlencoding(&cfg.target)
    );
    let (mut ws, _resp) = match tungstenite::connect(&url) {
        Ok(pair) => pair,
        Err(error) => return SessionEnd::Retry(format!("websocket: {error}")),
    };
    if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = ws.get_mut() {
        let _ = tcp.set_read_timeout(Some(Duration::from_millis(25)));
        let _ = tcp.set_nodelay(true);
    }
    let mut capture = match crate::caption_audio::Capture::start(cfg.source) {
        Ok(capture) => capture,
        Err(error) => {
            publish(shared, |view| {
                view.status = Status::Failed;
                view.error = Some(format!("采不到声音：{error}"));
                view.stage = "音频设备打不开".into();
            });
            return SessionEnd::Stopped;
        }
    };
    publish(shared, |view| {
        view.status = Status::Running;
        view.stage.clear();
        view.error = None;
        view.device = sidecar.device.clone();
        view.model = sidecar.model.clone();
    });

    let started = Instant::now();
    let mut last_frame = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = ws.send(tungstenite::Message::Binary(Vec::new().into()));
            let _ = ws.close(None);
            return SessionEnd::Stopped;
        }
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Stop => {
                    let _ = ws.send(tungstenite::Message::Binary(Vec::new().into()));
                    let _ = ws.close(None);
                    return SessionEnd::Stopped;
                }
                Cmd::Translate(on, target) => {
                    // 换目标语言需要重建侧车会话：断开让上层重连（cfg 已由调用方更新）。
                    let _ = on;
                    let _ = target;
                    let _ = ws.close(None);
                    return SessionEnd::Retry("切换到新的翻译语言".into());
                }
            }
        }
        // 先把攒下的音频全推过去。
        let mut pushed = 0usize;
        while let Ok(chunk) = capture.rx.try_recv() {
            if chunk.is_empty() {
                continue;
            }
            let mut bytes = Vec::with_capacity(chunk.len() * 2);
            for sample in chunk {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            if ws.send(tungstenite::Message::Binary(bytes.into())).is_err() {
                return SessionEnd::Retry("推流失败".into());
            }
            pushed += 1;
            if pushed >= 8 {
                break;
            }
        }
        match ws.read() {
            Ok(tungstenite::Message::Text(text)) => {
                last_frame = Instant::now();
                apply(shared, text.as_str());
            }
            Ok(tungstenite::Message::Binary(bytes)) => {
                last_frame = Instant::now();
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    apply(shared, text);
                }
            }
            Ok(tungstenite::Message::Ping(payload)) => {
                let _ = ws.send(tungstenite::Message::Pong(payload));
            }
            Ok(tungstenite::Message::Close(_)) => {
                let _ = ws.close(None);
                return SessionEnd::Retry("侧车关闭了连接".into());
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return SessionEnd::Retry(format!("{error}")),
        }
        // 侧车两分钟一句话都不说，当它死了（正常时每 200 ms 至少一份快照）。
        if last_frame.elapsed() > Duration::from_secs(120) {
            return SessionEnd::Retry("侧车没有响应".into());
        }
        // 启动超过 3 分钟还没进入 Running 说明模型下载卡住了，给个提示但不掐断。
        if started.elapsed() > Duration::from_secs(180) {
            publish(shared, |view| {
                if view.status == Status::Starting {
                    view.stage = "引擎启动比预期慢（可能在下载模型），继续等…".into();
                }
            });
        }
    }
}

fn apply(shared: &Arc<Mutex<Shared>>, text: &str) {
    // 实机联调时把每帧落到文件，方便无界面核对（不影响正常使用）。
    if let Some(path) = std::env::var_os("DAOZAI_CAPTION_DEBUG") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{text}");
        }
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    match value.get("type").and_then(|v| v.as_str()) {
        Some("config") => {
            // 侧车已就绪（真正开始出字前先到这一帧）。翻译模型还在后台
            // 加载时给出提示，免得用户以为翻译坏了。
            let translate_ready = value
                .get("translate_ready")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            publish(shared, |view| {
                view.status = Status::Running;
                if translate_ready {
                    view.stage.clear();
                } else {
                    view.stage = "翻译模型加载中…".into();
                }
            });
        }
        Some("captions") => {
            let lang = value.get("lang").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let lang_prob = value.get("lang_prob").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let partial = value
                .get("partial")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let mut lines = Vec::new();
            if let Some(items) = value.get("lines").and_then(|v| v.as_array()) {
                for item in items {
                    let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                    let id = item.get("id").and_then(|v| v.as_str()).map(str::to_string).unwrap_or_else(|| {
                        format!("{}:{}", item.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0), text.chars().take(12).collect::<String>())
                    });
                    lines.push(Line {
                        id,
                        text,
                        translation: item.get("translation").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        lang: item.get("lang").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        start: item.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                        end: item.get("end").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                        done: item.get("final").and_then(|v| v.as_bool()).unwrap_or(false),
                    });
                }
            }
            publish(shared, |view| {
                view.status = Status::Running;
                view.lang = lang;
                view.lang_prob = lang_prob;
                view.partial = partial;
                view.lines = lines;
                view.last_at = Some(Instant::now());
            });
        }
        Some("translation") => {
            let id = value.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let translation = value.get("translation").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if id.is_empty() || translation.is_empty() {
                return;
            }
            publish(shared, |view| {
                if let Some(line) = view.lines.iter_mut().find(|line| line.id == id) {
                    line.translation = translation;
                }
                view.last_at = Some(Instant::now());
            });
        }
        Some("error") => {
            let message = value.get("error").and_then(|v| v.as_str()).unwrap_or("未知错误").to_string();
            publish(shared, |view| {
                view.status = Status::Failed;
                view.error = Some(message.clone());
                view.stage = "侧车报错".into();
            });
        }
        Some("ready_to_stop") => {}
        _ => {}
    }
}

fn publish(shared: &Arc<Mutex<Shared>>, edit: impl FnOnce(&mut View)) {
    if let Ok(mut shared) = shared.lock() {
        edit(&mut shared.view);
    }
}

fn sleep_while(stop: &Arc<AtomicBool>, total: Duration) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 目标语言只是 `zh` / `ja` 这类短码，编码一下更稳（将来允许 `zh-Hant` 也不会坏）。
fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 语言码 → 岛上显示的名字。
pub fn language_name(code: &str) -> String {
    let lower = code.to_ascii_lowercase();
    let base = lower.split(['-', '_']).next().unwrap_or("");
    match base {
        "zh" => "中文",
        "en" => "英语",
        "ja" => "日语",
        "ko" => "韩语",
        "fr" => "法语",
        "de" => "德语",
        "es" => "西班牙语",
        "ru" => "俄语",
        "pt" => "葡萄牙语",
        "it" => "意大利语",
        "ar" => "阿拉伯语",
        "hi" => "印地语",
        "th" => "泰语",
        "vi" => "越南语",
        "id" => "印尼语",
        "tr" => "土耳其语",
        "nl" => "荷兰语",
        "pl" => "波兰语",
        "uk" => "乌克兰语",
        "ms" => "马来语",
        "yue" => "粤语",
        "" => "未知",
        other => other,
    }
    .to_string()
}

/// 语言码 → 显示的短标签（`EN` / `ZH`）。
pub fn language_tag(code: &str) -> String {
    let base = code.split(['-', '_']).next().unwrap_or("");
    if base.is_empty() {
        return "??".into();
    }
    base.to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_names_cover_common_codes() {
        assert_eq!(language_name("en"), "英语");
        assert_eq!(language_name("zh-CN"), "中文");
        assert_eq!(language_name("ja"), "日语");
        assert_eq!(language_tag("en"), "EN");
        assert_eq!(language_tag("zh-Hant"), "ZH");
        assert_eq!(language_tag(""), "??");
    }

    #[test]
    fn target_language_is_url_encoded() {
        assert_eq!(urlencoding("zh"), "zh");
        assert_eq!(urlencoding("zh-Hant"), "zh-Hant");
        assert_eq!(urlencoding("a b"), "a%20b");
    }

    #[test]
    fn captions_event_replaces_lines_and_keeps_id() {
        let captions = Captions::new();
        apply(
            &captions.shared,
            r#"{"type":"captions","seq":2,"status":"active","lang":"en","lang_prob":0.98,
                "lines":[{"id":"s1","text":"hello","translation":"","lang":"en","start":0.4,"end":1.2,"final":true}],
                "partial":"and then"}"#,
        );
        let view = captions.view();
        assert_eq!(view.status, Status::Running);
        assert_eq!(view.lang, "en");
        assert_eq!(view.lines.len(), 1);
        assert_eq!(view.lines[0].text, "hello");
        assert_eq!(view.partial, "and then");
        apply(
            &captions.shared,
            r#"{"type":"translation","id":"s1","translation":"你好","lang":"en"}"#,
        );
        assert_eq!(captions.view().lines[0].translation, "你好");
    }

    #[test]
    fn error_event_is_surfaced() {
        let captions = Captions::new();
        apply(&captions.shared, r#"{"type":"error","error":"模型加载失败"}"#);
        let view = captions.view();
        assert_eq!(view.status, Status::Failed);
        assert_eq!(view.error.as_deref(), Some("模型加载失败"));
    }

    #[test]
    fn garbage_does_not_panic_or_change_state() {
        let captions = Captions::new();
        apply(&captions.shared, "not json");
        apply(&captions.shared, r#"{"type":"unknown"}"#);
        assert_eq!(captions.view().status, Status::Off);
    }
}
