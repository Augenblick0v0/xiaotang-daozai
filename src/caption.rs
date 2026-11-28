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

/// 岛上渲染用的轻量快照。
#[derive(Clone, Debug, Default)]
pub struct IslandView {
    pub status: Status,
    pub translate: bool,
    pub lang: String,
    pub target: String,
    /// 最近一次云端翻译失败的原因；空表示没出错。
    pub translate_error: String,
    /// 最近三句（按时间顺序），只用于拼滚动文本窗。
    pub lines: Vec<Line>,
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
    /// 本次会话的完整记录：按 id 合并全部字幕（面板只摊最近几行，
    /// 记录页/导出要的是全量）。侧车每次发的是快照，不能直接替换历史。
    pub history: Vec<Line>,
    /// 当前这场会话在 `history` 里的起点；岛上只显示这场的新句子，
    /// 上一场留下的旧字幕留在记录页里，但不会一直挂在岛上。
    pub session_start: usize,
    /// 还没定稿的那半句（侧车的 buffer_transcription）。
    pub partial: String,
    pub error: Option<String>,
    /// 上一次云端翻译失败的原因；成功后清空。UI 用它替代一直转圈的「翻译中…」。
    pub translate_error: String,
    pub translate: bool,
    pub target: String,
    /// 侧车自报的 device（cuda / cpu），给页面显示用。
    pub device: String,
    pub model: String,
    /// 最近一次收到字幕快照的时刻（用于「多久没动静」）。
    pub last_at: Option<Instant>,
    /// 每次真正连上侧车加一，用来给行 id 加会话前缀，避免重连后撞上旧记录。
    pub conn: u64,
    /// 本连接里侧车的原始行 id → 历史行 id。
    pub ids: std::collections::HashMap<String, String>,
}

/// 一次启动需要的参数（从 `Settings` 拷过来，线程里不再碰 UI 状态）。
#[derive(Clone, Debug)]
pub struct StartConfig {
    /// 0 = 本机 Whisper 侧车；1 = Windows 实时字幕（UI Automation）。
    pub engine: u8,
    pub source: crate::caption_audio::CaptureSource,
    /// 识别语言：`zh` / `en` / `mixed` / `ja` / `ko`（mixed 走 auto）。
    pub lang: String,
    pub translate: bool,
    pub target: String,
    pub python: String,
    pub model: String,
    pub force_cpu: bool,
    pub beam: u8,
    /// 0 = 本机 NLLB 侧车；1 = Google 免费接口；2 = OpenAI 兼容接口。
    pub translate_backend: u8,
    pub api_url: String,
    pub api_key: String,
    pub api_model: String,
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

    /// 岛上每帧只需要的轻量快照：状态 + 最后三条文本。
    /// clone 整份 history（上限 2000 行）会让悬停展开动画明显掉帧。
    pub fn island(&self) -> IslandView {
        let shared = self.shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let view = &shared.view;
        let mut recent: Vec<Line> = Vec::new();
        for line in view.history.iter().skip(view.session_start).rev() {
            if line.text.trim().is_empty() { continue; }
            recent.push(line.clone());
            if recent.len() == 3 { break; }
        }
        recent.reverse();
        IslandView {
            status: view.status,
            translate: view.translate,
            lang: view.lang.clone(),
            target: view.target.clone(),
            translate_error: view.translate_error.clone(),
            lines: recent,
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
            // 换一次会话不清记录：上一场的句子还留在页面上，可以继续往回翻，
            // 真正要清空时用「清空」按钮。
            let history = std::mem::take(&mut shared.view.history);
            let session_start = history.len();
            shared.view = View {
                status: Status::Starting,
                stage: "正在准备识别引擎…".into(),
                translate: cfg.translate,
                target: cfg.target.clone(),
                model: cfg.model.clone(),
                history,
                session_start,
                ..Default::default()
            };
        }
        let thread = std::thread::Builder::new()
            .name("caption".into())
            .spawn(move || {
                if cfg.engine == 1 {
                    run_live(shared, stop_thread, rx, cfg);
                } else {
                    run(shared, stop_thread, rx, cfg);
                }
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

    /// 开机时把上次留下的记录灌进视图，页面一打开就能看见旧句子。
    /// 只在还没有任何实时结果时生效，避免把正在跑的会话覆盖掉。
    pub fn seed_history(&mut self, lines: Vec<Line>) {
        self.set(|view| {
            if view.history.is_empty() && !lines.is_empty() {
                view.lines = lines.iter().rev().take(6).rev().cloned().collect();
                view.history = lines;
            }
        });
    }

    /// 清空本次会话的记录（不动已落盘的历史，那份由页面自己的 History 管）。
    pub fn clear_history(&mut self) {
        self.set(|view| {
            view.history.clear();
            view.lines.clear();
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
            false,
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


/// 系统实时字幕（Windows Live Captions）引擎。
///
/// 识别由系统完成，这里只做三件事：读 UIA 文本 → 按句归档 → 需要时把定稿句子
/// 发给 translate-only 侧车翻译。翻译请求走单独线程，UIA 轮询不会被 NLLB 卡住。
fn run_live(
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<Cmd>,
    cfg: StartConfig,
) {
    let mut cfg = cfg;
    publish(&shared, |view| {
        view.status = Status::Starting;
        view.stage = "正在连接系统实时字幕…".into();
    });
    let mut live = match crate::live_captions::LiveCaptions::start() {
        Ok(live) => live,
        Err(error) => {
            publish(&shared, |view| {
                view.status = Status::Failed;
                view.stage = "系统实时字幕没起来".into();
                view.error = Some(error);
            });
            return;
        }
    };

    // 翻译服务：translate-only 侧车。翻译模型加载慢，先起在后台，字幕照常出。
    let mut sidecar: Option<crate::caption_runtime::Sidecar> = None;
    let mut jobs: Option<mpsc::Sender<(String, String, String)>> = None;
    let mut worker: Option<std::thread::JoinHandle<()>> = None;
    if cfg.translate && cfg.translate_backend == 0 {
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
            true,
            true,
            &stage,
            &stop,
        ) {
            Ok(sc) => {
                let url = format!("http://127.0.0.1:{}/translate", sc.port);
                let target = cfg.target.clone();
                let (tx, queue) = mpsc::channel::<(String, String, String)>();
                let tr_shared = shared.clone();
                worker = Some(std::thread::spawn(move || {
                    while let Ok(mut job) = queue.recv() {
                        // 翻译永远只追最新一句：慢的时候宁可跳过中间句，也不要排长队。
                        while let Ok(next) = queue.try_recv() {
                            job = next;
                        }
                        let (id, text, src) = job;
                        let body = serde_json::json!({
                            "text": text,
                            "src": src,
                            "tgt": target,
                        })
                        .to_string();
                        // 侧车刚起步时 NLLB 还在加载，POST 会 502；等模型热起来再试，
                        // 否则第一句永远没有译文。
                        let mut translated = None;
                        for attempt in 0..8 {
                            translated = ureq::post(&url)
                                .timeout(Duration::from_secs(120))
                                .set("Content-Type", "application/json")
                                .send_string(&body)
                                .ok()
                                .and_then(|response| response.into_string().ok())
                                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                                .and_then(|value| {
                                    value.get("translation").and_then(|v| v.as_str()).map(str::to_string)
                                });
                            if translated.is_some() {
                                break;
                            }
                            if attempt < 7 {
                                std::thread::sleep(Duration::from_secs(3));
                            }
                        }
                        if let Some(text) = translated {
                            publish(&tr_shared, |view| {
                                if let Some(line) =
                                    view.history.iter_mut().rev().find(|line| line.id == id)
                                {
                                    line.translation = text;
                                }
                            });
                        }
                    }
                }));
                jobs = Some(tx);
                sidecar = Some(sc);
            }
            Err(error) => {
                publish(&shared, |view| {
                    view.error = Some(format!("翻译不可用：{error}"));
                });
            }
        }
    }

    if cfg.translate && cfg.translate_backend != 0 {
        let target = cfg.target.clone();
        let backend = cfg.translate_backend;
        let url = cfg.api_url.clone();
        let key = cfg.api_key.clone();
        let model = cfg.api_model.clone();
        let (tx, queue) = mpsc::channel::<(String, String, String)>();
        let tr_shared = shared.clone();
        worker = Some(std::thread::spawn(move || {
            while let Ok(mut job) = queue.recv() {
                while let Ok(next) = queue.try_recv() {
                    job = next;
                }
                let (id, text, src) = job;
                match translate_remote(backend, &url, &key, &model, &text, &src, &target) {
                    Ok(translated) => publish(&tr_shared, |view| {
                        if let Some(line) = view.history.iter_mut().rev().find(|line| line.id == id)
                        {
                            line.translation = translated;
                        }
                        view.translate_error.clear();
                    }),
                    Err(error) => publish(&tr_shared, |view| view.translate_error = error),
                }
            }
        }));
        jobs = Some(tx);
    }

    publish(&shared, |view| {
        view.status = Status::Running;
        view.stage.clear();
        view.error = None;
        view.device = "系统".into();
        view.model = "实时字幕".into();
        view.translate = cfg.translate;
    });

    let mut last_text = String::new();
    // 还在增长的那半句只有一条：快照每来一次就更新它，不能每来一次就多一行。
    // 会话前缀必须唯一：live-part-1 这种短 id 跨会话会撞车，命中上一场的历史行后
    // 新句会被写到 session_start 之前，岛上永远只看到「正在听…」。
    let session = format!("{:08x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) as u32);
    let mut open_id: Option<String> = None;
    let mut open_seq = 0usize;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        while let Ok(command) = rx.try_recv() {
            match command {
                Cmd::Stop => {
                    stop.store(true, Ordering::SeqCst);
                }
                Cmd::Translate(on, target) => {
                    cfg.translate = on;
                    cfg.target = target;
                }
            }
        }
        let mut changed = false;
        while let Some(event) = live.try_recv() {
            match event {
                crate::live_captions::LiveEvent::Text(text) => {
                    if text == last_text {
                        continue;
                    }
                    last_text = text.clone();
                    let sentences = crate::live_captions::split_sentences(&text);
                    let count = sentences.len();
                    for (position, sentence) in sentences.into_iter().enumerate() {
                        let last = position + 1 == count;
                        let id = if last && !sentence.final_ {
                            if open_id.is_none() {
                                open_seq += 1;
                                open_id = Some(format!("live-{session}-part-{open_seq}"));
                            }
                            open_id.clone().unwrap_or_default()
                        } else if last && sentence.final_ {
                            open_id.take().unwrap_or_else(|| format!("live-{session}-{:08x}", fnv1a(&sentence.text)))
                        } else {
                            format!("live-{session}-{:08x}", fnv1a(&sentence.text))
                        };
                        let lang = detect_lang(&sentence.text).to_string();
                        // 已经在记录里且早已定稿的句子不再翻译：刚打开翻译时，
                        // 系统字幕会把前面几行一起送进来，不能把它们重新翻一遍。
                        let existed_done = shared
                            .lock()
                            .map(|shared| {
                                shared
                                    .view
                                    .history
                                    .iter()
                                    .enumerate()
                            .any(|(index, line)| index < shared.view.session_start && line.id == id && line.done)
                            })
                            .unwrap_or(false);
                        publish(&shared, |view| {
                            // 定稿句到达时，清掉同一文本的半句占位行，否则记录里
                            // 会同时留着「正在识别」和「已定稿」两份一样的字。
                            if sentence.final_ {
                                if let Some(index) = view.history.iter().position(|line| {
                                    line.id.contains("-part-") && line.text.trim() == sentence.text.trim()
                                }) {
                                    view.history.remove(index);
                                }
                            }
                            // 先摘掉旧行再追加：同一快照里多句时，半句被原地更新会
                            // 留在旧位置，历史顺序就反了（top/bottom 颠倒）。
                            let existing = view.history.iter().position(|line| line.id == id);
                            let mut line = existing.map(|index| view.history.remove(index)).unwrap_or_default();
                            line.id = id.clone();
                            line.text = sentence.text.clone();
                            line.done = sentence.final_;
                            line.lang = lang.clone();
                            if !sentence.final_ { line.translation.clear(); }
                            view.history.push(line);
                            view.lines =
                                view.history.iter().rev().take(6).rev().cloned().collect();
                            view.lang = lang.clone();
                            view.lang_prob = 1.0;
                            view.last_at = Some(Instant::now());
                            view.partial = if sentence.final_ {
                                String::new()
                            } else {
                                sentence.text.clone()
                            };
                        });
                        if sentence.final_ && !existed_done {
                            if let Some(jobs) = &jobs {
                                if cfg.translate && !same_language(&lang, &cfg.target) {
                                    let _ = jobs.send((id, sentence.text.clone(), lang));
                                }
                            }
                        }
                    }
                    changed = true;
                }
                crate::live_captions::LiveEvent::Ready => {
                    publish(&shared, |view| {
                        view.status = Status::Running;
                        view.stage.clear();
                        view.lang = detect_lang(&last_text).to_string();
                    });
                }
                crate::live_captions::LiveEvent::Error(error) => {
                    publish(&shared, |view| {
                        view.error = Some(error.clone());
                        view.stage = format!("系统实时字幕：{error}");
                    });
                }
            }
        }
        let _ = changed;
        std::thread::sleep(Duration::from_millis(60));
    }
    live.stop();
    drop(jobs);
    if let Some(worker) = worker {
        let _ = worker.join();
    }
    drop(sidecar);
}

/// 稳定 id：同一句在不同快照里得到同一个 hash，历史里就不会重复。
fn fnv1a(text: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// 系统实时字幕不报语言；按字符集做最小判断，够岛上显示 `[ZH]` / `[EN]` 用。
fn detect_lang(text: &str) -> &'static str {
    let mut han = 0usize;
    let mut kana = 0usize;
    let mut hangul = 0usize;
    let mut latin = 0usize;
    for ch in text.chars() {
        match ch as u32 {
            0x3040..=0x30ff => kana += 1,
            0xac00..=0xd7af => hangul += 1,
            0x4e00..=0x9fff => han += 1,
            _ if ch.is_ascii_alphabetic() => latin += 1,
            _ => {}
        }
    }
    if kana > 0 {
        "ja"
    } else if hangul > 0 {
        "ko"
    } else if han > latin {
        "zh"
    } else {
        "en"
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
    // 「中英混合」就是逐段自动判定；显式固定一种语言能少很多误判。
    let language = match cfg.lang.trim() {
        "" | "mixed" => "auto",
        other => other,
    };
    let url = format!(
        "ws://127.0.0.1:{}/asr?language={}&translate={}&target_language={}",
        sidecar.port,
        urlencoding(language),
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
    // 新连接意味着侧车从零开始编号；给 id 加连接前缀，否则会和上一场的历史撞车。
    publish(shared, |view| {
        view.conn += 1;
        view.ids.clear();
        view.session_start = view.history.len();
    });
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
                // 侧车给的是「当前缓冲区的快照」：整段话会被反复重发，
                // 直接覆盖会让记录只剩最后几行。按 id 并进历史，
                // 同一句增长时原地更新，新句才追加。
                for mut line in lines {
                    if line.text.trim().is_empty() {
                        continue;
                    }
                    let raw_id = line.id.clone();
                    let mapped = view
                        .ids
                        .entry(raw_id.clone())
                        .or_insert_with(|| format!("{}-{raw_id}", view.conn))
                        .clone();
                    line.id = mapped;
                    if let Some(existing) = view.history.iter_mut().find(|old| old.id == line.id) {
                        existing.text = line.text;
                        if !line.translation.trim().is_empty() {
                            existing.translation = line.translation;
                        }
                        existing.lang = line.lang;
                        existing.done = line.done;
                        existing.end = existing.end.max(line.end);
                    } else {
                        view.history.push(line);
                    }
                }
                dedupe_history(&mut view.history, view.session_start);
                if view.history.len() > 2000 {
                    let drop = view.history.len() - 2000;
                    view.history.drain(0..drop);
                    // 记录裁掉最旧的行时，本场会话的起点要跟着往前挪，
                    // 否则 index 会永远指在列表末尾之外，岛上一条也不显示。
                    view.session_start = view.session_start.saturating_sub(drop);
                }
                view.lines = view
                    .history
                    .iter()
                    .skip(view.session_start)
                    .rev()
                    .take(6)
                    .rev()
                    .cloned()
                    .collect();
                if let Ok(path) = std::env::var("DAOZAI_CAPTION_DEBUG") {
                    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                        use std::io::Write;
                        let _ = writeln!(file, "{{\"view\":true,\"session_start\":{},\"history\":{},\"lines\":{}}}", view.session_start, view.history.len(), view.lines.len());
                    }
                }
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
                let id = view.ids.get(&id).cloned().unwrap_or(id);
                if let Some(line) = view.lines.iter_mut().find(|line| line.id == id) {
                    line.translation = translation.clone();
                }
                if let Some(line) = view.history.iter_mut().find(|line| line.id == id) {
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

/// 同一句话在 WLK 里可能换一个 id 再发一遍（缓冲裁剪后会重排），
/// 直接按 id 合并就会在记录里留下两条一模一样的句子。
/// 这里按「忽略空白和标点」的文本再收一次，保留最新那条；新的一条没有译文时
/// 沿用旧译文，避免译文被重发冲掉。
fn dedupe_history(history: &mut Vec<Line>, from: usize) {
    fn key(text: &str) -> String {
        text.chars()
            .filter(|c| !c.is_whitespace() && !matches!(c, '.' | ',' | '!' | '?' | ';' | ':' | '。' | '，' | '！' | '？' | '；' | '：' | '、' | '…' | '\u{201c}' | '\u{201d}' | '\u{2018}' | '\u{2019}'))
            .flat_map(char::to_lowercase)
            .collect()
    }

    // 只收本场会话那一段：旧记录是已经落过盘的，一帧里再合并会把
    // 它们的条数变少，session_start 就指到列表外面去，岛上会一条都不显示。
    let from = from.min(history.len());
    let mut tail: Vec<Line> = history.split_off(from);
    // 从后往前扫：先见到的是最新的一份，重复的旧句子只用来补译文。
    // 用 HashMap 而不是两两比较，2000 行的记录每帧也不会变成 O(n²)。
    let mut latest: std::collections::HashMap<String, Line> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for line in tail.drain(..).rev() {
        let text_key = key(&line.text);
        if text_key.is_empty() {
            continue;
        }
        if let Some(existing) = latest.get_mut(&text_key) {
            if existing.translation.trim().is_empty() {
                existing.translation = line.translation;
            }
            continue;
        }
        order.push(text_key.clone());
        latest.insert(text_key, line);
    }
    let mut kept = Vec::with_capacity(order.len());
    for text_key in order.into_iter().rev() {
        if let Some(line) = latest.remove(&text_key) {
            kept.push(line);
        }
    }
    history.extend(kept);
}



/// 云端翻译：不依赖本机 NLLB，适合本机速度跟不上时使用。
pub fn translate_remote(
    backend: u8,
    base_url: &str,
    key: &str,
    model: &str,
    text: &str,
    src: &str,
    tgt: &str,
) -> Result<String, String> {
    if text.trim().is_empty() {
        return Ok(String::new());
    }
    if backend == 1 {
        let url = format!(
            "https://translate.googleapis.com/translate_a/single?client=gtx&sl={}&tl={}&dt=t&q={}",
            urlencoding(src),
            urlencoding(tgt),
            urlencoding(text)
        );
        let body = ureq::get(&url)
            .timeout(Duration::from_secs(12))
            .call()
            .map_err(|err| format!("Google 翻译请求失败：{err}"))?
            .into_string()
            .map_err(|err| format!("Google 翻译读取失败：{err}"))?;
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|err| format!("Google 翻译返回格式不对：{err}"))?;
        let first = value.get(0).and_then(|v| v.get(0)).and_then(|v| v.get(0)).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        return if first.is_empty() { Err("Google 翻译返回空结果".into()) } else { Ok(first) };
    }
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("接口地址为空".into());
    }
    let url = format!("{base}/chat/completions");
    let body = serde_json::json!({
        "model": model,
        "temperature": 0.2,
        "stream": true,
        "messages": [
            {"role": "system", "content": format!("Translate the user's text from {src} to {tgt}. Reply with the translation only, no quotes, no explanation.")},
            {"role": "user", "content": text}
        ]
    })
    .to_string();
    let mut request = ureq::post(&url)
        .timeout(Duration::from_secs(30))
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream");
    if !key.trim().is_empty() {
        request = request.set("Authorization", &format!("Bearer {}", key.trim()));
    }
    let response = match request.send_string(&body) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let detail = response.into_string().unwrap_or_default();
            let detail = detail.trim();
            return Err(format!("接口返回 {status}{}", if detail.is_empty() { String::new() } else { format!("：{}", detail.chars().take(160).collect::<String>()) }));
        }
        Err(err) => return Err(format!("请求接口失败：{err}")),
    };
    let raw = response.into_string().map_err(|err| format!("读取接口响应失败：{err}"))?;
    // 流式响应是若干 data: {...} 行；有些兼容服务会忽略 stream 直接回整段 JSON。
    let mut out = String::new();
    let mut saw_json = false;
    for line in raw.lines() {
        let line = line.trim();
        let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if payload.is_empty() || payload == "[DONE]" { continue; }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else { continue; };
        saw_json = true;
        if let Some(delta) = value.pointer("/choices/0/delta/content").and_then(|v| v.as_str()) {
            out.push_str(delta);
        } else if let Some(content) = value.pointer("/choices/0/message/content").and_then(|v| v.as_str()) {
            out.push_str(content);
        }
    }
    let out = out.trim().to_string();
    if !saw_json {
        return Err(format!("接口没有返回 JSON：{}", raw.trim().chars().take(160).collect::<String>()));
    }
    if out.is_empty() { Err("接口返回了空译文".into()) } else { Ok(out) }
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

/// 源语言与目标语言是不是同一种（`zh-Hans` 与 `zh` 算同一种）。
/// 同语言时翻译没有意义，NLLB 还会把中文「翻」成另一句中文，必须跳过。
pub fn same_language(source: &str, target: &str) -> bool {
    let base = |code: &str| -> String {
        let lower = code.trim().to_ascii_lowercase();
        lower.split(['-', '_', '>']).next().unwrap_or("").to_string()
    };
    let src = base(source);
    // mixed = auto，识别器逐段判定，不能预判同语。
    if src.is_empty() || src == "auto" || src == "mixed" || target.trim().is_empty() {
        return false;
    }
    src == base(target)
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
    fn stopping_keeps_the_transcript_on_screen() {
        let mut captions = Captions::new();
        apply(
            &captions.shared,
            r#"{"type":"captions","seq":1,"status":"active","lang":"en","lang_prob":0.9,
                "lines":[{"id":"s1","text":"hello","translation":"","lang":"en","start":0.0,"end":1.0,"final":true}]}"#,
        );
        captions.stop();
        let view = captions.view();
        assert_eq!(view.status, Status::Off);
        assert_eq!(view.history.len(), 1, "停止之后记录要留在屏幕上");
        assert_eq!(view.history[0].text, "hello");
    }

    #[test]
    fn garbage_does_not_panic_or_change_state() {
        let captions = Captions::new();
        apply(&captions.shared, "not json");
        apply(&captions.shared, r#"{"type":"unknown"}"#);
        assert_eq!(captions.view().status, Status::Off);
    }

    fn sample_line(id: &str, text: &str, translation: &str) -> Line {
        Line { id: id.into(), text: text.into(), translation: translation.into(), lang: "zh".into(), start: 0.0, end: 1.0, done: true }
    }

    #[test]
    fn history_keeps_one_copy_of_a_sentence_that_changed_id() {
        let mut history = vec![
            sample_line("a", "今天我们要讨论发布计划。", ""),
            sample_line("b", "别的句子。", ""),
            sample_line("c", "今天我们要讨论发布计划", "今天我们要讨论发布计划。"),
        ];
        dedupe_history(&mut history, 0);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].id, "c");
        assert_eq!(history[1].translation, "今天我们要讨论发布计划。");
    }

    #[test]
    fn a_new_session_keeps_the_island_feed_alive() {
        let mut captions = Captions::new();
        captions.seed_history(vec![
            sample_line("a", "上一场的第一句。", ""),
            sample_line("b", "上一场的第二句。", ""),
        ]);
        captions.start(StartConfig {
            engine: 0,
            source: crate::caption_audio::CaptureSource::SystemLoopback,
            lang: "zh".into(),
            translate: false,
            target: "en".into(),
            python: String::new(),
            model: "base".into(),
            force_cpu: true,
            beam: 1,
            translate_backend: 0,
            api_url: String::new(),
            api_key: String::new(),
            api_model: String::new(),
        });
        // start() 之后 session_start 必须等于已有的历史条数，
        // 第一帧新字幕不能被旧记录挡住。
        assert_eq!(captions.view().session_start, 2);
        apply(
            &captions.shared,
            r#"{"type":"captions","seq":1,"status":"active","lang":"zh","lines":[{"id":"L0000-00000000-00","text":"新的一场开始了。","translation":"","lang":"zh","start":"0:00:00.00","end":"0:00:01.00","final":true}]}"#,
        );
        let view = captions.view();
        assert_eq!(view.lines.len(), 1);
        assert_eq!(view.lines[0].text, "新的一场开始了。");
    }

    #[test]
    fn old_duplicates_do_not_shrink_the_part_before_the_session() {
        // 上一场存下来的记录本身就有重复。去重只收拾本场那一段，
        // 否则条数一变少，session_start 就指到列表外面，岛上永远空着。
        let mut history = vec![
            sample_line("old-1", "上一场的一句话。", ""),
            sample_line("old-2", "上一场的一句话。", ""),
        ];
        dedupe_history(&mut history, 2);
        assert_eq!(history.len(), 2, "旧记录不能被本场去重吃掉");

        history.push(sample_line("new-1", "新的一场开始了。", ""));
        dedupe_history(&mut history, 2);
        assert_eq!(history.len(), 3);
        assert_eq!(history.last().map(|l| l.text.as_str()), Some("新的一场开始了。"));
    }

    #[test]
    fn seeded_history_shows_before_a_new_session() {
        let mut captions = Captions::new();
        captions.seed_history(vec![sample_line("a", "上一场的一句话。", "")]);
        assert_eq!(captions.view().history.len(), 1);
        assert_eq!(captions.view().lines.len(), 1);
        captions.clear_history();
        assert!(captions.view().history.is_empty());
    }
}
