//! 「实时字幕」页：本机采音 → WhisperLiveKit 识别 → 岛上显示原文语言与原文，
//! 打开翻译后同一行下面再显示目标语言的译文。
//!
//! 页面上不出现「模型」「推理」这类词：用户只关心「听没听到 / 听的是什么语言 /
//! 翻成了什么」。技术细节（设备、模型、侧车日志）收在最下面一行小字里。
use crate::app::*;
pub(crate) use crate::caption::language_tag;
use crate::caption::{language_name, StartConfig, Status};
use crate::caption_audio::CaptureSource;
use crate::paint::{Card, H};
use crate::theme::{self, size, W};
use eframe::egui::{self, Color32};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 页面上最多显示几行（旧的往上顶掉）。
const ROWS: usize = 3;
/// 译文行的浅色。
const CAPTION_FOOTER_Y: f32 = 246.0;
/// 译文行的浅色。
const TRANSLATION: Color32 = Color32::from_rgb(150, 214, 226);

/// 可选的目标语言：短码 + 岛上显示名。只放常用的，不做两百语言的菜单。
/// 识别语言：短码 + 页面显示名 + 胶囊宽度。`mixed` 交给 auto 逐段判定，
/// 但默认不选它——固定中文能明显减少「听成日文/英文」。
const LANGS: [(&str, &str, f32); 5] = [
    ("zh", "中文", 40.0),
    ("en", "English", 44.0),
    ("mixed", "中英", 38.0),
    ("ja", "日本語", 44.0),
    ("ko", "한국어", 46.0),
];

const TARGETS: [(&str, &str); 4] = [("zh", "中文"), ("en", "English"), ("ja", "日本語"), ("ko", "한국어")];

/// 安装运行环境的进度（后台线程写，页面每帧读）。
#[derive(Clone, Default)]
pub(crate) struct InstallProgress {
    pub running: bool,
    pub stage: String,
    pub error: Option<String>,
    pub finished: bool,
}

/// 「安装识别环境」按钮背后的线程。
pub(crate) struct Installer {
    shared: Arc<Mutex<InstallProgress>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Default for Installer {
    fn default() -> Self {
        Self::new()
    }
}

impl Installer {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(InstallProgress::default())),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    pub fn view(&self) -> InstallProgress {
        self.shared.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn busy(&self) -> bool {
        self.thread.is_some() && self.view().running
    }

    pub fn start(&mut self) {
        if self.busy() {
            return;
        }
        {
            let mut progress = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            *progress = InstallProgress { running: true, stage: "正在准备…".into(), ..Default::default() };
        }
        let shared = self.shared.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        self.stop = stop;
        self.thread = std::thread::Builder::new()
            .name("caption-install".into())
            .spawn(move || {
                let stage = |text: &str| {
                    if let Ok(mut progress) = shared.lock() {
                        progress.stage = text.to_string();
                    }
                };
                let result = crate::caption_runtime::install(&stage, &stop_thread);
                if let Ok(mut progress) = shared.lock() {
                    progress.running = false;
                    progress.finished = result.is_ok();
                    progress.error = result.err();
                }
            })
            .ok();
    }

    pub fn cancel(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl App {
    /// 每帧对齐「设置里想不想开」和「实际在不在跑」。
    fn epoch_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    pub(crate) fn caption_tick(&mut self) {
        // 设置里改了上限就立刻生效（改小会顺手裁掉最旧的）。
        let cap = (self.settings.caption_history_mb as usize).max(1) * 1024 * 1024;
        self.caption_history.set_cap_bytes(cap);
        let wanted = self.settings.caption_enabled;
        if !wanted {
            if self.caption.active() {
                self.caption.stop();
                self.caption_cfg = None;
            }
            return;
        }
        let source = if self.settings.caption_source == 1 {
            CaptureSource::Microphone
        } else {
            CaptureSource::SystemLoopback
        };
        // 旧版本允许过 720，用户设置里可能还留着越界值；这里就地夹回画布范围。
        self.settings.caption_bar_width = self.settings.caption_bar_width.clamp(360, 1128);
        let target = self.settings.caption_target_lang.clone();
        let lang = self.settings.caption_lang.clone();
        // 源语言和目标语言相同时翻译没有意义（NLLB 会给出另一句中文），
        // 这里只关掉这次会话的翻译，别改用户的开关。
        let translate = self.settings.caption_translate
            && !crate::caption::same_language(&lang, &target);
        let cfg = StartConfig {
            engine: self.settings.caption_engine.min(1),
            source,
            lang: lang.clone(),
            translate,
            target: target.clone(),
            python: self.settings.caption_python.clone(),
            model: self.settings.caption_model.clone(),
            force_cpu: self.settings.caption_device == 1,
            beam: self.settings.caption_beam.max(1),
            translate_backend: self.settings.caption_translate_backend.min(2),
            api_url: self.settings.caption_api_url.clone(),
            api_key: self.settings.caption_api_key.clone(),
            api_model: self.settings.caption_api_model.clone(),
        };
        let changed = self.caption_cfg.as_ref().is_some_and(|old| {
            old.engine != cfg.engine
                || old.source != cfg.source
                || old.lang != cfg.lang
                || old.translate != cfg.translate
                || old.target != cfg.target
                || old.python != cfg.python
                || old.model != cfg.model
                || old.force_cpu != cfg.force_cpu
                || old.beam != cfg.beam
                || old.translate_backend != cfg.translate_backend
                || old.api_url != cfg.api_url
                || old.api_key != cfg.api_key
                || old.api_model != cfg.api_model
        });
        if !self.caption.active() {
            // 只在「还没起过」时拉起一次。线程挂了（侧车报错、端口冲突）就停在
            // Failed 上让用户看见，别每帧重启——那会让岛在两种高度之间来回跳。
            if self.caption_cfg.is_none() {
                self.caption.start(cfg.clone());
            }
            self.caption_cfg = Some(cfg.clone());
        } else if changed {
            self.caption.stop();
            self.caption.start(cfg.clone());
            self.caption_cfg = Some(cfg.clone());
        }
        // 安装完成后自动接着启动（用户刚装完不该再点一次）。
        if !self.caption.active() && self.caption_cfg.is_none() && self.caption_install.view().finished {
            self.caption.start(cfg.clone());
            self.caption_cfg = Some(cfg.clone());
        }
        // 把新定稿的字幕并进落盘记录（按 id 去重；攒够一批再写，别每帧刷盘）。
        let view = self.caption.view();
        let mut dirty = false;
        for line in &view.history {
            let translation = line.translation.clone();
            if self.caption_saved.get(&line.id) == Some(&translation) {
                continue;
            }
            self.caption_history.record(crate::caption_history::Entry {
                id: line.id.clone(),
                text: line.text.clone(),
                translation,
                lang: line.lang.clone(),
                start: line.start,
                end: line.end,
            });
            self.caption_saved.insert(line.id.clone(), line.translation.clone());
            dirty = true;
        }
        if dirty {
            let _ = self.caption_history.flush();
        }
    }

    pub(crate) fn page_caption(&mut self, c: &Card, _env: &Env, _cmds: &mut Vec<Cmd>) {
        let view = self.caption.view();
        let install = self.caption_install.view();
        // 标题：和其它自绘页一样，「实时」白 + 「字幕」青。
        c.text(15.0, 25.0, "实时", size::TITLE, W::Bold, theme::WHITE, H::Left);
        let width = c.text_w("实时", size::TITLE, W::Bold);
        c.text(15.0 + width + 1.5, 25.0, "字幕", size::TITLE, W::Bold, theme::CYAN, H::Left);

        // 右上角状态灯：一眼看出「在听 / 在启动 / 停了 / 出错」。
        let (tint, label) = match view.status {
            Status::Running => (theme::SOFT_GREEN, "识别中"),
            Status::Starting => (theme::YELLOW, "启动中"),
            Status::Failed => (theme::RED, "出错"),
            Status::Off => (theme::DIM, "已停止"),
        };
        c.circle(296.0, 22.0, 3.0, tint);
        c.text(304.0, 25.0, label, 10.0, W::Semibold, tint, H::Left);

        let mut changed = false;

        // 第一行：音源 / 翻译开关 / 启停。
        let mut x = 15.0;
        if self.settings.caption_engine == 1 {
            // 系统实时字幕自己决定听哪个设备，这里不再摆假开关。
            c.text(15.0, 46.0, "声音由系统「实时字幕」接管", 9.5, W::Regular, theme::DIM, H::Left);
            x = 152.0;
        } else {
            let source_labels = [("系统声音", 0u8), ("麦克风", 1u8)];
            for (text, value) in source_labels {
                let on = self.settings.caption_source == value;
                let (hit, inner) = crate::paint::capsule(
                    c,
                    x,
                    36.0,
                    58.0,
                    20.0,
                    &format!("cap-source-{value}"),
                    if on { theme::CYAN.linear_multiply(0.32) } else { Color32::from_white_alpha(12) },
                    if on { theme::CYAN.linear_multiply(0.42) } else { Color32::from_white_alpha(22) },
                );
                inner.centered_text(x + 29.0, 46.0, text, 10.0, W::Regular, if on { theme::WHITE } else { theme::GRAY }, H::Center);
                if hit.clicked() && !on {
                    self.settings.caption_source = value;
                    changed = true;
                }
                x += 62.0;
            }
        }
        let translate_on = self.settings.caption_translate;
        let (hit, inner) = crate::paint::capsule(
            c,
            x,
            36.0,
            48.0,
            20.0,
            "cap-translate",
            if translate_on { theme::GREEN.linear_multiply(0.35) } else { Color32::from_white_alpha(12) },
            if translate_on { theme::GREEN.linear_multiply(0.45) } else { Color32::from_white_alpha(22) },
        );
        inner.centered_text(x + 24.0, 46.0, if translate_on { "译文" } else { "原文" }, 10.0, W::Regular, if translate_on { theme::WHITE } else { theme::GRAY }, H::Center);
        if hit.clicked() {
            self.settings.caption_translate = !translate_on;
            changed = true;
        }
        let running = self.settings.caption_enabled;
        let (hit, inner) = crate::paint::capsule(
            c,
            355.0,
            36.0,
            52.0,
            20.0,
            "cap-toggle",
            if running { Color32::from_white_alpha(16) } else { theme::CYAN.linear_multiply(0.5) },
            if running { Color32::from_white_alpha(26) } else { theme::CYAN.linear_multiply(0.62) },
        );
        inner.centered_text(381.0, 46.0, if running { "停止" } else { "开启" }, 10.0, W::Semibold, theme::WHITE, H::Center);
        if hit.clicked() {
            self.settings.caption_enabled = !running;
            changed = true;
        }

        if self.settings.caption_engine == 1 {
            // 系统实时字幕的语言在它自己的设置里选，这里只提示，不做假开关。
            c.text(15.0, 76.0, "语言由系统「实时字幕」设置决定", 9.5, W::Regular, theme::DIM, H::Left);
        } else {
            // 第二行：识别语言。固定语言比 auto 稳：auto 会把中文听成日文。
            c.text(15.0, 76.0, "识别", 9.5, W::Regular, theme::DIM, H::Left);
            let mut lx = 38.0;
            let mut lang_hit = false;
            for (code, name, chip_w) in LANGS {
                let on = self.settings.caption_lang == code;
                let (hit, inner) = crate::paint::capsule(
                    c,
                    lx,
                    62.0,
                    chip_w,
                    20.0,
                    &format!("cap-lang-{code}"),
                    if on { theme::CYAN.linear_multiply(0.32) } else { Color32::from_white_alpha(10) },
                    if on { theme::CYAN.linear_multiply(0.42) } else { Color32::from_white_alpha(20) },
                );
                inner.centered_text(lx + chip_w / 2.0, 72.0, name, 9.5, W::Regular, if on { theme::WHITE } else { theme::DIM }, H::Center);
                if hit.clicked() && !on {
                    self.settings.caption_lang = code.to_string();
                    changed = true;
                    lang_hit = true;
                }
                lx += chip_w + 4.0;
            }
            let _ = lang_hit;
        }

        // 导出 / 清空：记录是本机文件，导出到桌面（TXT 给复制，SRT 给剪辑）。
        let (hit, inner) = crate::paint::capsule(c, 296.0, 62.0, 50.0, 20.0, "cap-export",
            Color32::from_white_alpha(12), Color32::from_white_alpha(24));
        inner.centered_text(321.0, 72.0, "导出", 9.5, W::Regular, theme::GRAY, H::Center);
        if hit.clicked() {
            let message = self.caption_export(&view);
            self.caption_notice = Some((message, std::time::Instant::now()));
        }
        let clear_armed = self.caption_clear_at.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(5));
        let (hit, inner) = crate::paint::capsule(c, 350.0, 62.0, 50.0, 20.0, "cap-clear",
            if clear_armed { theme::RED.linear_multiply(0.4) } else { Color32::from_white_alpha(12) },
            Color32::from_white_alpha(24));
        inner.centered_text(375.0, 72.0, if clear_armed { "确认" } else { "清空" }, 9.5, W::Regular, if clear_armed { theme::WHITE } else { theme::GRAY }, H::Center);
        if hit.clicked() {
            if clear_armed {
                self.caption.clear_history();
                self.caption_history.clear();
                let _ = self.caption_history.save();
                self.caption_clear_at = None;
                self.caption_notice = Some(("已清空本次记录".to_string(), std::time::Instant::now()));
            } else {
                self.caption_clear_at = Some(std::time::Instant::now());
            }
        }

        // 第三行：译文目标语言（只在开了翻译时出现）。
        let mut list_top = 90.0;
        if self.settings.caption_translate {
            c.text(15.0, 104.0, "译成", 9.5, W::Regular, theme::DIM, H::Left);
            let mut tx = 42.0;
            for (code, name) in TARGETS {
                let on = self.settings.caption_target_lang == code;
                let (hit, inner) = crate::paint::capsule(
                    c,
                    tx,
                    88.0,
                    60.0,
                    20.0,
                    &format!("cap-target-{code}"),
                    if on { theme::CYAN.linear_multiply(0.3) } else { Color32::from_white_alpha(10) },
                    if on { theme::CYAN.linear_multiply(0.42) } else { Color32::from_white_alpha(20) },
                );
                inner.centered_text(tx + 30.0, 98.0, name, 9.5, W::Regular, if on { theme::WHITE } else { theme::DIM }, H::Center);
                if hit.clicked() && !on {
                    self.settings.caption_target_lang = code.to_string();
                    changed = true;
                }
                tx += 64.0;
            }
            list_top = 114.0;
        }

        // 上一场的记录还在时优先把记录铺出来，别用引导文案把它盖住。
        if view.status == Status::Off && !self.settings.caption_enabled && view.history.is_empty() {
            let top = list_top;
            c.text(15.0, top + 18.0, "把正在放的声音变成字幕", 12.5, W::Semibold, theme::WHITE, H::Left);
            let hint = c.fit(
                "点右上角「开启」：岛上会实时显示原文和它是什么语言；打开翻译还能同时显示译文。声音只在本机处理，不上传。",
                10.5,
                W::Regular,
                380.0,
            );
            c.text(15.0, top + 36.0, &hint, 10.5, W::Regular, theme::GRAY, H::Left);
            let ready = crate::caption_runtime::installed();
            let setup = if ready {
                "识别环境已就绪，点「开启」开始"
            } else {
                "需要先装一次识别环境（约 2.5 GB，只装一次）"
            };
            c.text(15.0, top + 66.0, setup, 10.0, W::Regular, theme::DIM, H::Left);
            let (hit, inner) = crate::paint::capsule(
                c,
                15.0,
                top + 78.0,
                if ready { 88.0 } else { 132.0 },
                22.0,
                "cap-install",
                if ready { Color32::from_white_alpha(14) } else { theme::CYAN.linear_multiply(0.5) },
                Color32::from_white_alpha(30),
            );
            let label = if ready {
                "环境就绪".to_string()
            } else if install.running {
                "正在安装…".to_string()
            } else {
                "安装识别环境".to_string()
            };
            inner.centered_text(15.0 + if ready { 44.0 } else { 66.0 }, top + 89.0, &label, 10.5, W::Semibold, theme::WHITE, H::Center);
            if hit.clicked() && !ready && !install.running {
                self.caption_install.start();
            }
            if !install.stage.is_empty() {
                let stage = c.fit(&install.stage, 9.5, W::Regular, 380.0);
                c.text(15.0, top + 110.0, &stage, 9.5, W::Regular, theme::DIM, H::Left);
            }
            if let Some(error) = &install.error {
                let text = c.fit(error, 9.5, W::Regular, 380.0);
                c.text(15.0, top + 124.0, &text, 9.5, W::Regular, theme::RED, H::Left);
            }
            self.caption_footer(c, &view);
            return;
        }

        // 记录区：可滚动，滚到底跟随最新一句。侧车每帧发的是快照，
        // 历史在 caption.rs 里按 id 合并过，这里画的是全量。
        let list_h = (CAPTION_FOOTER_Y - 22.0 - list_top).max(40.0);
        let list_rect = c.rect(12.0, list_top, 396.0, list_h);
        let mut content = egui::Ui::new(
            c.ui.ctx().clone(),
            egui::Id::new(("caption-history", c.live)),
            egui::UiBuilder::new()
                .max_rect(list_rect)
                .layer_id(c.painter.layer_id())
                .style(c.ui.style().clone()),
        );
        content.set_clip_rect(list_rect.intersect(c.painter.clip_rect()));
        content.set_opacity(c.painter.opacity());
        content.spacing_mut().item_spacing.y = 0.0;
        content.spacing_mut().scroll.floating = false;
        content.spacing_mut().scroll.bar_width = 4.0 * c.s;

        let show_translation = self.settings.caption_translate;
        let row_h = if show_translation { 34.0 } else { 19.0 };
        let history = &view.history;
        if history.is_empty() {
            let waiting = match view.status {
                Status::Starting => view.stage.clone(),
                Status::Failed => view.error.clone().unwrap_or_else(|| "识别引擎出错了".into()),
                _ => "在听…（说话就会出字）".to_string(),
            };
            if !waiting.is_empty() {
                let waiting = c.fit(&waiting, 11.0, W::Regular, 380.0);
                c.text(15.0, list_top + 24.0, &waiting, 11.0, W::Regular, if view.status == Status::Failed { theme::RED } else { theme::DIM }, H::Left);
            }
        } else {
            let mut area = egui::ScrollArea::vertical()
                .id_salt("caption-history-scroll")
                .auto_shrink([false, false]);
            // 跟随新句子时交给 egui 自己贴底；手动翻过页才用记下来的偏移。
            // 之前用 f32::MAX/2 当「到底」，egui 会因为偏移越界直接 panic。
            area = if self.caption_follow {
                area.stick_to_bottom(true)
            } else {
                area.vertical_scroll_offset(self.caption_scroll)
            };
            let output = area.show_rows(&mut content, row_h * c.s, history.len(), |ui, range| {
                    for index in range {
                        let line = &history[index];
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(392.0 * c.s, row_h * c.s), egui::Sense::hover());
                        let mut row = Card::new(ui, rect.min, ui.clip_rect());
                        row.s = c.s;
                        row.live = c.live;
                        let tag = if line.lang.is_empty() { language_tag(&view.lang) } else { language_tag(&line.lang) };
                        row.text(0.0, 11.0, &tag, 9.0, W::Semibold, theme::CYAN, H::Left);
                        let original = if line.text.trim().is_empty() { line.translation.clone() } else { line.text.clone() };
                        let text = row.fit(&original, 12.0, W::Regular, 356.0);
                        row.text(22.0, 11.0, &text, 12.0, W::Regular, if line.done { theme::WHITE } else { theme::GRAY }, H::Left);
                        if show_translation {
                            let translated = if line.translation.trim().is_empty() {
                                // 只有「本场最后一句、还在跑」才值得显示占位；
                                // 上一场留下来的老记录没有译文就是没有，别装作还在翻。
                                if view.status == Status::Running && index + 1 == history.len() {
                                    "翻译中…".to_string()
                                } else {
                                    String::new()
                                }
                            } else {
                                line.translation.clone()
                            };
                            if !translated.is_empty() {
                                let dialog = row.fit(&translated, 10.5, W::Regular, 356.0);
                                row.text(22.0, 27.0, &dialog, 10.5, W::Regular, TRANSLATION, H::Left);
                            }
                        }
                    }
                });
            // 用户刚用滚轮往下翻过：记住位置；停在底部时继续自动跟随新句子。
            self.caption_scroll = output.state.offset.y;
            let max = (output.content_size.y - output.inner_rect.height()).max(0.0);
            self.caption_max_scroll = max;
            if self.caption_follow {
                // 跟随新句：把记录位置钉在底部，滚轮向上翻时才交给手动偏移。
                self.caption_scroll = max;
            } else {
                self.caption_scroll = self.caption_scroll.clamp(0.0, max);
                if self.caption_scroll >= max - 2.0 {
                    self.caption_follow = true;
                }
            }
        }

        // 正在说的那半句：浅灰，跟在列表后面，不进历史。
        if self.settings.caption_show_partial && !view.partial.trim().is_empty() {
            let partial = c.fit(&view.partial, 10.5, W::Regular, 360.0);
            c.text(34.0, CAPTION_FOOTER_Y - 16.0, &partial, 10.5, W::Regular, theme::DIM, H::Left);
        }

        // 导出/清空的结果提示，显示 6 秒。
        if let Some((message, at)) = &self.caption_notice {
            if at.elapsed() < std::time::Duration::from_secs(6) {
                let text = c.fit(message, 9.5, W::Regular, 300.0);
                c.text(15.0, CAPTION_FOOTER_Y - 2.0, &text, 9.5, W::Regular, theme::SOFT_GREEN, H::Left);
            }
        }

        self.caption_footer(c, &view);

        if view.status == Status::Running {
            c.ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
        let _ = changed;
    }

    /// 把这次（含以前几次）的记录导出成桌面上的 TXT + SRT。
    /// 返回给页面显示的短提示；失败也照实说，不静默。
    fn caption_export(&mut self, view: &crate::caption::View) -> String {
        let dir = std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .map(|home| home.join("Desktop"))
            .filter(|desktop| desktop.is_dir())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let txt = dir.join(format!("字幕记录-{stamp}.txt"));
        let srt = dir.join(format!("字幕记录-{stamp}.srt"));
        // 记录可能刚从内存并入还没落盘；导出前先刷一次，保证两边一致。
        let _ = self.caption_history.flush();
        let mut result = if self.caption_history.is_empty() {
            // 极端情况：记录模块还没收到任何条目，但视图里已有内容。
            let _ = view;
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "还没有可导出的记录"))
        } else {
            Ok(())
        };
        if result.is_ok() {
            result = self.caption_history.export_txt(&txt).and_then(|_| self.caption_history.export_srt(&srt));
        }
        match result {
            Ok(()) => format!("已导出到桌面（{} 句）", self.caption_history.len()),
            Err(error) => format!("导出失败：{error}"),
        }
    }

    fn caption_footer(&self, c: &Card, view: &crate::caption::View) {
        let mut parts: Vec<String> = Vec::new();
        if !view.lang.is_empty() {
            parts.push(format!("原文 {}", language_name(&view.lang)));
        }
        if view.translate && !view.target.is_empty() {
            parts.push(format!("译文 {}", language_name(&view.target)));
        }
        parts.push(format!("{} 句", view.history.len()));
        let text = parts.join(" · ");
        c.text(15.0, CAPTION_FOOTER_Y, &text, 9.5, W::Regular, theme::DIM, H::Left);
        // 右侧小字：设备/模型，出错时换成错误原文的头部。
        let right = if let Some(error) = &view.error {
            c.fit(error, 9.5, W::Regular, 210.0)
        } else if !view.device.is_empty() {
            format!("{} · {}", view.device, view.model)
        } else {
            String::new()
        };
        if !right.is_empty() {
            c.text(404.0, CAPTION_FOOTER_Y, &right, 9.5, W::Regular, theme::DIM, H::Right);
        }
    }

    pub(crate) fn caption_settings(&mut self, ui: &mut egui::Ui) {
        use crate::extras::{setting_row, settings_card, settings_label, settings_segmented};
        let installed = crate::caption_runtime::installed();
        settings_label(ui, "识别环境");
        settings_card(ui, |ui| {
            ui.label(
                egui::RichText::new(if installed {
                    "已装好。识别和翻译都在本机跑，音频不出这台电脑。"
                } else {
                    "还没装。首次需要下载约 2.5 GB 的识别与翻译模型（只装一次）。"
                })
                .size(11.5)
                .color(egui::Color32::from_gray(150)),
            );
            if !installed {
                let progress = self.caption_install.view();
                if progress.running {
                    ui.label(egui::RichText::new(&progress.stage).size(11.0).color(egui::Color32::from_gray(170)));
                } else if ui.button("安装识别环境").clicked() {
                    self.caption_install.start();
                }
                if let Some(error) = &progress.error {
                    ui.label(egui::RichText::new(error).size(11.0).color(egui::Color32::from_rgb(255, 120, 120)));
                }
            }
        });
        settings_label(ui, "字幕");
        settings_card(ui, |ui| {
            setting_row(ui, "开启实时字幕", "打开后按下面的音源持续识别人声", &mut self.settings.caption_enabled);
            ui.separator();
            setting_row(ui, "跟随岛启动", "岛仔启动后自动开始听（否则只记住开关状态）", &mut self.settings.caption_autostart);
            ui.separator();
            ui.label(egui::RichText::new("音源").size(11.5).color(egui::Color32::from_gray(150)));
            settings_segmented(ui, &mut self.settings.caption_engine, &[(0u8, "本机 Whisper"), (1, "系统实时字幕")], 110.0);
            if self.settings.caption_engine == 1 {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("岛宽度").size(11.0));
                    ui.add(egui::Slider::new(&mut self.settings.caption_bar_width, 360..=1128).suffix(" px"));
                });
            }
        ui.add_space(6.0);
        settings_label(ui, "声音来源（Whisper 引擎）");
        settings_segmented(ui, &mut self.settings.caption_source, &[(0u8, "系统声音"), (1, "麦克风")], 86.0);
            ui.separator();
            ui.label(egui::RichText::new("识别语言").size(11.5).color(egui::Color32::from_gray(150)));
            let mut lang: &str = match self.settings.caption_lang.as_str() {
                "en" => "en",
                "mixed" => "mixed",
                "ja" => "ja",
                "ko" => "ko",
                _ => "zh",
            };
            if settings_segmented(ui, &mut lang, &[("zh", "中文"), ("en", "English"), ("mixed", "中英"), ("ja", "日本語"), ("ko", "한국어")], 62.0) {
                self.settings.caption_lang = lang.to_string();
            }
            ui.label(
                egui::RichText::new("固定一种语言比「自动」准得多；中文会要求模型输出简体。")
                    .size(11.0)
                    .color(egui::Color32::from_gray(140)),
            );
            ui.separator();
            setting_row(ui, "显示未定稿的半句", "把还在识别中的那半句用浅色显示出来", &mut self.settings.caption_show_partial);
            ui.separator();
            setting_row(ui, "收起时在岛上显示", "岛收起成药丸时，在角标位置滚动显示最近一句", &mut self.settings.caption_on_island);
            setting_row(ui, "跟随自动避让", "被前台应用遮住时，字幕区也一起收成细线", &mut self.settings.caption_avoid);
        });
        settings_label(ui, "翻译");
        settings_card(ui, |ui| {
            setting_row(ui, "开启翻译", "把定稿的句子翻成目标语言，和原文一起显示", &mut self.settings.caption_translate);
            ui.separator();
            ui.label(egui::RichText::new("目标语言").size(11.5).color(egui::Color32::from_gray(150)));
            let mut code: &str = match self.settings.caption_target_lang.as_str() {
                "en" => "en",
                "ja" => "ja",
                "ko" => "ko",
                _ => "zh",
            };
            if settings_segmented(ui, &mut code, &[("zh", "中文"), ("en", "English"), ("ja", "日本語"), ("ko", "한국어")], 74.0) {
                self.settings.caption_target_lang = code.to_string();
            }
            ui.separator();
            ui.label(egui::RichText::new("翻译服务").size(11.5).color(egui::Color32::from_gray(150)));
            settings_segmented(ui, &mut self.settings.caption_translate_backend, &[(0u8, "本机 NLLB"), (1, "Google 免费"), (2, "自定义 API")], 92.0);
            if self.settings.caption_translate_backend == 2 {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("接口地址").size(11.0));
                    ui.add(egui::TextEdit::singleline(&mut self.settings.caption_api_url).desired_width(220.0).hint_text("https://api.openai.com/v1"));
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("API Key").size(11.0));
                    ui.add(egui::TextEdit::singleline(&mut self.settings.caption_api_key).desired_width(220.0).password(true));
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("模型").size(11.0));
                    ui.add(egui::TextEdit::singleline(&mut self.settings.caption_api_model).desired_width(220.0).hint_text("gpt-4o-mini"));
                });
                ui.label(egui::RichText::new("兼容 OpenAI /chat/completions 的服务都能填；Key 只存在本机设置文件里。").size(10.5).color(egui::Color32::from_gray(140)));
            }
        });
        settings_label(ui, "记录");
        settings_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("最多保存");
                ui.add(egui::DragValue::new(&mut self.settings.caption_history_mb).range(1..=500).speed(1.0));
                ui.label("MB 的字幕记录");
            });
            ui.label(
                egui::RichText::new("每次识别的原文和译文都会存下来，超过上限自动删最旧的；字幕页可以随时导出 TXT/SRT。")
                    .size(11.0)
                    .color(egui::Color32::from_gray(140)),
            );
        });
        settings_label(ui, "识别引擎");
        settings_card(ui, |ui| {
            ui.label(egui::RichText::new("模型").size(11.5).color(egui::Color32::from_gray(150)));
            let mut model: &str = if self.settings.caption_model == "small" { "small" } else { "base" };
            if settings_segmented(ui, &mut model, &[("base", "base · 快"), ("small", "small · 准")], 96.0) {
                self.settings.caption_model = model.to_string();
            }
            ui.separator();
            ui.label(egui::RichText::new("算力").size(11.5).color(egui::Color32::from_gray(150)));
            settings_segmented(ui, &mut self.settings.caption_device, &[(0u8, "自动（有显卡用显卡）"), (1, "只用 CPU")], 118.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Python 解释器");
                ui.add(egui::TextEdit::singleline(&mut self.settings.caption_python).hint_text("留空 = 用装好的环境"));
            });
            ui.label(
                egui::RichText::new("装好的环境默认在 %LOCALAPPDATA%\\daozai\\wlk\\venv。换机器或想用别的 Python 时才需要填。")
                    .size(11.0)
                    .color(egui::Color32::from_gray(140)),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_all_lowercase_codes() {
        for (code, _) in TARGETS {
            assert!(code.chars().all(|c| c.is_ascii_lowercase()));
        }
    }
}


