//! 「实时字幕」页：本机采音 → WhisperLiveKit 识别 → 岛上显示原文语言与原文，
//! 打开翻译后同一行下面再显示目标语言的译文。
//!
//! 页面上不出现「模型」「推理」这类词：用户只关心「听没听到 / 听的是什么语言 /
//! 翻成了什么」。技术细节（设备、模型、侧车日志）收在最下面一行小字里。
use crate::app::*;
pub(crate) use crate::caption::language_tag;
use crate::caption::{language_name, Line, StartConfig, Status};
use crate::caption_audio::CaptureSource;
use crate::paint::{Card, H};
use crate::theme::{self, size, W};
use eframe::egui::{self, Color32};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 页面上最多显示几行（旧的往上顶掉）。
const ROWS: usize = 3;
/// 译文行的浅色。
const CAPTION_FOOTER_Y: f32 = 214.0;
/// 译文行的浅色。
const TRANSLATION: Color32 = Color32::from_rgb(150, 214, 226);

/// 可选的目标语言：短码 + 岛上显示名。只放常用的，不做两百语言的菜单。
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
    pub(crate) fn caption_tick(&mut self) {
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
        let target = self.settings.caption_target_lang.clone();
        let cfg = StartConfig {
            source,
            translate: self.settings.caption_translate,
            target: target.clone(),
            python: self.settings.caption_python.clone(),
            model: self.settings.caption_model.clone(),
            force_cpu: self.settings.caption_device == 1,
            beam: self.settings.caption_beam.max(1),
        };
        let changed = self.caption_cfg.as_ref().is_some_and(|old| {
            old.source != cfg.source
                || old.translate != cfg.translate
                || old.target != cfg.target
                || old.python != cfg.python
                || old.model != cfg.model
                || old.force_cpu != cfg.force_cpu
                || old.beam != cfg.beam
        });
        if !self.caption.active() {
            self.caption.start(cfg.clone());
            self.caption_cfg = Some(cfg.clone());
        } else if changed {
            self.caption.stop();
            self.caption.start(cfg.clone());
            self.caption_cfg = Some(cfg.clone());
        }
        // 安装完成后自动接着启动（用户刚装完不该再点一次）。
        if !self.caption.active() && self.caption_install.view().finished {
            self.caption.start(cfg.clone());
            self.caption_cfg = Some(cfg.clone());
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
        // 状态灯收在标题右侧：右上角是设置齿轮，别压上去；原文语种在页脚显示。
        c.circle(296.0, 22.0, 3.0, tint);
        c.text(304.0, 25.0, label, 10.0, W::Semibold, tint, H::Left);

        // 控制条：音源 / 翻译 / 启停。
        let mut changed = false;
        let source_labels = [("系统声音", 0u8), ("麦克风", 1u8)];
        let mut x = 15.0;
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
        // 翻译开关：只占一个短胶囊，目标语言放到第二行选，避免一行挤六个胶囊。
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
        inner.centered_text(
            x + 24.0,
            46.0,
            if translate_on { "译文" } else { "原文" },
            10.0,
            W::Regular,
            if translate_on { theme::WHITE } else { theme::GRAY },
            H::Center,
        );
        if hit.clicked() {
            self.settings.caption_translate = !translate_on;
            changed = true;
        }
        if self.settings.caption_translate {
            c.text(15.0, 73.0, "译成", 9.5, W::Regular, theme::DIM, H::Left);
            let mut tx = 42.0;
            for (code, name) in TARGETS {
                let on = self.settings.caption_target_lang == code;
                let (hit, inner) = crate::paint::capsule(
                    c,
                    tx,
                    60.0,
                    60.0,
                    20.0,
                    &format!("cap-target-{code}"),
                    if on { theme::CYAN.linear_multiply(0.3) } else { Color32::from_white_alpha(10) },
                    if on { theme::CYAN.linear_multiply(0.42) } else { Color32::from_white_alpha(20) },
                );
                inner.centered_text(tx + 30.0, 70.0, name, 9.5, W::Regular, if on { theme::WHITE } else { theme::DIM }, H::Center);
                if hit.clicked() && !on {
                    self.settings.caption_target_lang = code.to_string();
                    changed = true;
                }
                tx += 64.0;
            }
        }
        // 启停按钮贴在右边。
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

        // 中间：字幕本体。
        let top = if self.settings.caption_translate { 88.0 } else { 64.0 };
        let rows = if self.settings.caption_translate { 2 } else { ROWS };
        if view.status == Status::Off && !self.settings.caption_enabled {
            // 没开：说明能干什么，别摆一个空框。
            c.text(15.0, top + 26.0, "把正在放的声音变成字幕", 12.5, W::Semibold, theme::WHITE, H::Left);
            let hint = c.fit(
                "点右上角「开启」：岛上会实时显示原文和它是什么语言；打开翻译还能同时显示译文。声音只在本机处理，不上传。",
                10.5,
                W::Regular,
                380.0,
            );
            c.text(15.0, top + 44.0, &hint, 10.5, W::Regular, theme::GRAY, H::Left);
            c.text(15.0, top + 78.0, "需要先装一次识别环境（约 2.5 GB，只装一次）", 10.0, W::Regular, theme::DIM, H::Left);
            let ready = crate::caption_runtime::installed();
            let (hit, inner) = crate::paint::capsule(
                c,
                15.0,
                top + 90.0,
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
            inner.centered_text(
                15.0 + if ready { 44.0 } else { 66.0 },
                top + 101.0,
                &label,
                10.5,
                W::Semibold,
                theme::WHITE,
                H::Center,
            );
            if hit.clicked() && !ready && !install.running {
                self.caption_install.start();
            }
            if !install.stage.is_empty() {
                let stage = c.fit(&install.stage, 9.5, W::Regular, 380.0);
                c.text(15.0, top + 122.0, &stage, 9.5, W::Regular, theme::DIM, H::Left);
            }
            if let Some(error) = &install.error {
                let text = c.fit(error, 9.5, W::Regular, 380.0);
                c.text(15.0, top + 136.0, &text, 9.5, W::Regular, theme::RED, H::Left);
            }
            self.caption_footer(c, &view);
            return;
        }

        // 有内容：把最近几行摊开（新的在下面）。
        let shown: Vec<&Line> = view
            .lines
            .iter()
            .filter(|line| !line.text.trim().is_empty())
            .rev()
            .take(rows)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let mut y = top + 4.0;
        if shown.is_empty() {
            let waiting = match view.status {
                Status::Starting => view.stage.clone(),
                Status::Failed => view.error.clone().unwrap_or_else(|| "识别引擎出错了".into()),
                _ => {
                    if view.partial.trim().is_empty() {
                        "在听…（说话就会出字）".to_string()
                    } else {
                        String::new()
                    }
                }
            };
            if !waiting.is_empty() {
                let waiting = c.fit(&waiting, 11.0, W::Regular, 380.0);
                c.text(15.0, top + 30.0, &waiting, 11.0, W::Regular, if view.status == Status::Failed { theme::RED } else { theme::DIM }, H::Left);
            }
        }
        for line in shown {
            let tag = if line.lang.is_empty() { language_tag(&view.lang) } else { language_tag(&line.lang) };
            let original = if line.text.trim().is_empty() { line.translation.clone() } else { line.text.clone() };
            let text = c.fit(&original, 12.0, W::Regular, 352.0);
            c.text(15.0, y + 11.0, &tag, 9.0, W::Semibold, theme::CYAN, H::Left);
            c.text(38.0, y + 11.0, &text, 12.0, W::Regular, theme::WHITE, H::Left);
            y += 17.0;
            if self.settings.caption_translate {
                let translated = if line.translation.trim().is_empty() {
                    if line.done {
                        "翻译中…".to_string()
                    } else {
                        String::new()
                    }
                } else {
                    line.translation.clone()
                };
                if !translated.is_empty() && y + 10.0 < CAPTION_FOOTER_Y - 8.0 {
                    let dialog = c.fit(&translated, 11.0, W::Regular, 352.0);
                    c.text(38.0, y + 10.0, &dialog, 11.0, W::Regular, TRANSLATION, H::Left);
                    y += 15.0;
                }
            }
            y += 6.0;
        }
        // 正在说的那半句：浅灰、只占最后一行，不参与历史。
        if !self.settings.caption_translate && self.settings.caption_show_partial && !view.partial.trim().is_empty() {
            let partial = c.fit(&view.partial, 11.0, W::Regular, 352.0);
            if y < 200.0 {
                c.text(38.0, y + 10.0, &partial, 11.0, W::Regular, theme::DIM, H::Left);
            }
        }
        self.caption_footer(c, &view);

        // 字幕是流式的：在跑就按 200 ms 催重绘，别等鼠标动。
        if view.status == Status::Running {
            c.ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
        let _ = changed;
    }

    fn caption_footer(&self, c: &Card, view: &crate::caption::View) {
        let mut parts: Vec<String> = Vec::new();
        if !view.lang.is_empty() {
            parts.push(format!("原文 {}", language_name(&view.lang)));
        }
        if view.translate && !view.target.is_empty() {
            parts.push(format!("译文 {}", language_name(&view.target)));
        }
        parts.push(format!("{} 句", view.lines.len()));
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
            settings_segmented(ui, &mut self.settings.caption_source, &[(0u8, "系统声音"), (1, "麦克风")], 86.0);
            ui.separator();
            setting_row(ui, "显示未定稿的半句", "把还在识别中的那半句用浅色显示出来", &mut self.settings.caption_show_partial);
            ui.separator();
            setting_row(ui, "收起时在岛上显示", "岛收起成药丸时，在角标位置滚动显示最近一句", &mut self.settings.caption_on_island);
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


