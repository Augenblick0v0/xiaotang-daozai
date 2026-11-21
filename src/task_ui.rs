//! 「AI 任务」页：把本机 Codex / Claude / DeepSeek 会话正在做的东西搬上岛。
//!
//! 数据全部来自本机已经落盘的会话记录（`task_scan` 只读文件、不联网），
//! 进度只在能真实算出来时才画：DeepSeek 的待办、Codex 的 `update_plan`、
//! Claude 的 `TodoWrite` 都算「客户端自己报上来的计划」，拿不到就画一段转圈的弧，
//! 不假装知道百分比——「不知道」比「编一个」有用。
use crate::app::*;
use crate::paint::{self, Card, H};
use crate::task_scan::{Source, Stage, TaskItem};
use crate::theme::{self, W};
use eframe::egui::{self, Color32};
use std::f32::consts::{FRAC_PI_2, TAU};

/// 需要你回复/确认时统一用琥珀色（和额度页的告警色一致）。
const AMBER: Color32 = Color32::from_rgb(255, 166, 77);
/// 出错。
const FAIL: Color32 = Color32::from_rgb(255, 92, 92);
/// 第一行任务的基线，与页头（25）留出一点距离。
const ROW_TOP: f32 = 42.0;
/// 行距：4 行正好放进 232 高的页面。
const ROW_H: f32 = 42.0;
/// 页面最多显示几行（再多的靠「N 个在跑」汇总，避免把岛撑爆）。
const ROWS: usize = 4;

impl App {
    /// 每帧调用：只有页面（或它的设置分页）开着的时候才去读盘。
    pub(crate) fn task_tick(&mut self) {
        if !self.settings.visible_pages[TASK_PAGE] {
            return;
        }
        let watching =
            (self.open && self.page == TASK_PAGE) || (self.settings_open && self.settings_tab == 15);
        if !watching {
            return;
        }
        let cfg = crate::task_scan::ScanConfig {
            sources: self.settings.task_sources,
            keep_minutes: self.settings.task_keep_minutes,
            now_ms: now_ms(),
        };
        self.task.tick(std::time::Instant::now(), cfg);
    }

    pub(crate) fn page_task(&mut self, c: &Card, _env: &Env, cmds: &mut Vec<Cmd>) {
        let now = now_ms();
        let items: Vec<TaskItem> = self.task.items().to_vec();
        if items.is_empty() {
            c.text(34.0, 70.0, "没有正在跑的任务", 13.0, W::Bold, theme::WHITE, H::Left);
            let hint = c.fit(
                "Codex CLI、Claude Code 或 DeepSeek Harness 跑起来以后，这里会显示它们在做什么",
                11.0,
                W::Regular,
                PW - 96.0,
            );
            c.text(34.0, 91.0, &hint, 11.0, W::Regular, theme::GRAY, H::Left);
            // 后台扫描是异步的：空列表时慢一点重绘，等结果回来。
            c.ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            return;
        }
        let mut alive = 0usize;
        let mut waiting = 0usize;
        for (index, task) in items.iter().take(ROWS).enumerate() {
            if task.stage.alive() {
                alive += 1;
            }
            if task.stage.needs_user() {
                waiting += 1;
            }
            let y = ROW_TOP + index as f32 * ROW_H;
            let accent = if task.stage.needs_user() {
                AMBER
            } else if task.stage == Stage::Failed {
                FAIL
            } else {
                theme::WHITE
            };
            // 左边给翻页箭头留一条空档：它画在页高正中，正好压在行首。
            let row = c.hit(40.0, y - 6.0, PW - 52.0, ROW_H - 4.0, ("task-row", task.id.as_str()));
            let (hover, _) = c.hover_t(&row);
            if hover > 0.01 {
                c.fill(40.0, y - 6.0, PW - 52.0, ROW_H - 4.0, 8.0, Color32::from_white_alpha((hover * 10.0) as u8));
            }
            source_logo(c, task.source, 52.0, y + 11.0, 16.0);
            let text_x = 68.0;
            let text_w = PW - 64.0 - text_x - 12.0;
            let title = c.fit(&task.title, 12.0, W::Semibold, text_w);
            c.text(text_x, y + 10.0, &title, 12.0, W::Semibold, accent, H::Left);
            // 标题已经退回到项目名时，副标题里就别再重复一遍。
            let hide_project = self.settings.task_hide_project || task.project == task.title;
            let sub = fitted_subtitle(c, task, now, hide_project, text_w);
            c.text(text_x, y + 25.0, &sub, 10.0, W::Regular, theme::GRAY, H::Left);
            ring(c, task, PW - 64.0, y + 11.0, self.t as f32);
            let tip = match &task.log {
                Some(log) => format!("{}｜{}｜点击打开会话记录", task.source.label(), subtitle(task, now, self.settings.task_hide_project)),
                None => format!("{}｜{}", task.source.label(), subtitle(task, now, self.settings.task_hide_project)),
            };
            if row.clone().on_hover_text(tip).clicked() {
                if let Some(log) = &task.log {
                    cmds.push(Cmd::Reveal(log.clone()));
                }
            }
        }
        if items.len() > ROWS {
            alive += items[ROWS..].iter().filter(|task| task.stage.alive()).count();
            waiting += items[ROWS..].iter().filter(|task| task.stage.needs_user()).count();
        }
        let summary = if waiting > 0 {
            format!("{alive} 个在跑 · {waiting} 个等你")
        } else if alive > 0 {
            format!("{alive} 个在跑")
        } else {
            format!("{} 个已结束", items.len())
        };
        c.text(15.0, TASK_H - 16.0, &summary, 10.0, W::Regular, theme::DIM, H::Left);
        // 有任务在跑/在等你时，让转圈的弧和用时继续走。
        if alive + waiting > 0 {
            c.ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    pub(crate) fn task_settings(&mut self, ui: &mut egui::Ui) {
        use crate::extras::{setting_row, settings_card, settings_label, settings_segmented};
        settings_label(ui, "任务来源");
        settings_card(ui, |ui| {
            setting_row(ui, "DeepSeek Harness", "读取本机会话缓存，不联网", &mut self.settings.task_sources[0]);
            ui.separator();
            setting_row(ui, "Codex CLI", "读取 ~/.codex 下的会话记录", &mut self.settings.task_sources[1]);
            ui.separator();
            setting_row(ui, "Claude Code", "读取 ~/.claude 下的会话记录", &mut self.settings.task_sources[2]);
        });
        settings_label(ui, "显示");
        settings_card(ui, |ui| {
            settings_segmented(
                ui,
                &mut self.settings.task_keep_minutes,
                &[(5u32, "5 分钟"), (15, "15 分钟"), (30, "半小时"), (60, "1 小时"), (240, "4 小时")],
                74.0,
            );
            ui.label(
                egui::RichText::new("只显示这段时间内有过活动的会话；更早的会话不再占位。")
                    .size(11.5)
                    .color(Color32::from_gray(150)),
            );
            ui.separator();
            setting_row(ui, "隐藏项目名", "共享屏幕时不显示工作目录名", &mut self.settings.task_hide_project);
        });
        settings_label(ui, "说明");
        settings_card(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "岛仔只读本机已经写好的会话记录，不联网、不上传。\
                     进度来自客户端自己报的待办清单：DeepSeek Harness 的待办、\
                     Codex 的 update_plan、Claude Code 的 TodoWrite；\
                     客户端没报计划时就显示成转圈的弧，不猜百分比。\
                     需要你回复或确认的会话标成琥珀色。",
                )
                .size(11.5)
                .color(Color32::from_gray(155)),
            );
        });
    }
}

/// 当前时间（UTC 毫秒）。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 「2 分 10 秒」这种给人看的时长；一行位置有限，超过一天就只说天数。
fn age_text(ms: i64) -> String {
    let seconds = (ms.max(0) / 1000) as u64;
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 600 {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    } else if seconds < 3600 {
        format!("{} 分", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} 小时", seconds / 3600)
    } else {
        format!("{} 天", seconds / 86_400)
    }
}

/// 「刚刚 / 3 分钟前」。
fn ago_text(ms: i64) -> String {
    let seconds = (ms.max(0) / 1000) as u64;
    if seconds < 30 {
        "刚刚".to_string()
    } else if seconds < 3600 {
        // 30~59 秒不要显示成「0 分钟前」。
        let minutes = seconds / 60;
        if minutes == 0 { format!("{seconds} 秒前") } else { format!("{minutes} 分钟前") }
    } else if seconds < 86_400 {
        format!("{} 小时前", seconds / 3600)
    } else {
        format!("{} 天前", seconds / 86_400)
    }
}

/// 第二行：阶段 · 模型 · 项目 · 用时/速度。
/// 一行放不下时从末尾往前丢（速度 → 项目 → 模型），阶段永远留着。
fn subtitle(task: &TaskItem, now: i64, hide_project: bool) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(5);
    parts.push(task.stage.label().to_string());
    // 8.5 点的圆环看不清百分比，有真实进度时顺手写在阶段后面。
    if let Some(progress) = live_progress(task) {
        parts.push(percent_text(progress));
    }
    if !task.model.is_empty() {
        parts.push(task.model.clone());
    }
    if !hide_project && !task.project.is_empty() {
        parts.push(task.project.clone());
    }
    if task.stage.needs_user() {
        // 等你的时长比「跑了多久」更有用：它反映拖了多久没处理。
        parts.push(format!("已等 {}", age_text(now - task.updated_ms)));
    } else if task.stage.alive() {
        parts.push(alive_text(task, now));
        if task.rate >= 1.0 {
            parts.push(format!("{:.0} tok/s", task.rate));
        }
    } else {
        parts.push(format!("{}结束", ago_text(now - task.updated_ms)));
        if task.output_tokens > 0 {
            parts.push(format!("{} tok", task.output_tokens));
        }
    }
    parts.join(" · ")
}

/// 「62%」：客户端报的完成比例。
fn percent_text(progress: f32) -> String {
    format!("{:.0}%", progress.clamp(0.0, 1.0) * 100.0)
}

/// 只有还在跑/还在等你的时候才拿计划进度来说事：已经结束的会话留着上一次的 10%，
/// 说「已完成 · 10%」只会让人觉得没做完。
fn live_progress(task: &TaskItem) -> Option<f32> {
    if task.stage.alive() || task.stage.needs_user() {
        task.progress
    } else {
        None
    }
}

/// 「已跑 3 分」；遇到一轮开了好几天、其实还在动的会话，说「N 分钟前更新」更实在。
fn alive_text(task: &TaskItem, now: i64) -> String {
    let running = now - task.started_ms;
    if running > 12 * 3600 * 1000 {
        format!("{}更新", ago_text(now - task.updated_ms))
    } else {
        format!("已跑 {}", age_text(running))
    }
}

/// 按宽度挑一个放得下的副标题：从最长的组合开始，放不下就丢掉最靠后的部分。
fn fitted_subtitle(c: &Card, task: &TaskItem, now: i64, hide_project: bool, max_w: f32) -> String {
    let full = subtitle(task, now, hide_project);
    if c.text_w(&full, 10.0, W::Regular) <= max_w {
        return full;
    }
    // 结尾那一段（速度/词元数）最先丢，其次是项目名，最后连模型也去掉，只留阶段。
    let mut keep_project = !hide_project;
    let mut keep_model = true;
    loop {
        let trimmed = subtitle_parts(task, now, keep_project && !task.project.is_empty(), keep_model).join(" · ");
        if c.text_w(&trimmed, 10.0, W::Regular) <= max_w {
            return trimmed;
        }
        if keep_project {
            keep_project = false;
        } else if keep_model && !task.model.is_empty() {
            keep_model = false;
        } else {
            return trimmed;
        }
    }
}

/// 副标题的各段，去掉末尾的「速度/词元」段（那一段在窄行里最先牺牲）。
fn subtitle_parts(task: &TaskItem, now: i64, with_project: bool, with_model: bool) -> Vec<String> {
    let mut parts: Vec<String> = Vec::with_capacity(4);
    parts.push(task.stage.label().to_string());
    if let Some(progress) = live_progress(task) {
        parts.push(percent_text(progress));
    }
    if with_model && !task.model.is_empty() {
        parts.push(task.model.clone());
    }
    if with_project && !task.project.is_empty() {
        parts.push(task.project.clone());
    }
    if task.stage.needs_user() {
        parts.push(format!("已等 {}", age_text(now - task.updated_ms)));
    } else if task.stage.alive() {
        parts.push(alive_text(task, now));
    } else {
        parts.push(format!("{}结束", ago_text(now - task.updated_ms)));
    }
    parts
}

/// 右侧圆环：有真实进度画进度，没有就画一段转圈的弧。
fn ring(c: &Card, task: &TaskItem, cx: f32, cy: f32, t: f32) {
    let radius = 8.5;
    let color = if task.stage.needs_user() {
        AMBER
    } else if task.stage == Stage::Failed {
        FAIL
    } else if task.stage.alive() {
        theme::SOFT_GREEN
    } else {
        Color32::from_white_alpha(90)
    };
    c.ring(cx, cy, radius, 2.0, Color32::from_white_alpha(24));
    match live_progress(task) {
        Some(progress) => {
            let progress = progress.clamp(0.0, 1.0);
            if progress > 0.002 {
                c.arc(cx, cy, radius, -FRAC_PI_2, -FRAC_PI_2 + TAU * progress, 2.0, color);
            }
        }
        None if task.stage.alive() || task.stage.needs_user() => {
            // 没有可信进度：一段 97° 的弧绕圈转，速度跟着时间走。
            let start = (t * 2.4) % TAU - FRAC_PI_2;
            c.arc(cx, cy, radius, start, start + 1.7, 2.0, color);
        }
        None => {}
    }
    if task.stage == Stage::Done {
        paint::check(c, cx, cy, 6.0, 1.0, 1.7, color);
    }
}

/// 来源图标：复用额度页那套 ai-icons，DeepSeek 保留品牌蓝，其余染白。
fn source_logo(c: &Card, source: Source, x: f32, y: f32, size: f32) {
    let ctx = c.ui.ctx().clone();
    let ppp = ctx.pixels_per_point();
    let pixels = (size * c.s * ppp).round().max(1.0) as u32;
    let Some(texture) = source_texture(&ctx, source, pixels) else {
        return;
    };
    let center = c.pos(x, y);
    let half = pixels as f32 / 2.0;
    let min = egui::pos2((center.x * ppp - half).round() / ppp, (center.y * ppp - half).round() / ppp);
    let rect = egui::Rect::from_min_size(min, egui::Vec2::splat(pixels as f32 / ppp));
    c.painter.image(texture.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
}

fn source_texture(ctx: &egui::Context, source: Source, pixels: u32) -> Option<egui::TextureHandle> {
    let key = egui::Id::new(("task-logo", source.key(), pixels));
    if let Some(texture) = ctx.data_mut(|data| data.get_temp::<egui::TextureHandle>(key)) {
        return Some(texture);
    }
    let bytes: &[u8] = match source {
        Source::Dsh => include_bytes!("../assets/ai-icons/deepseek.svg"),
        Source::Codex => include_bytes!("../assets/ai-icons/codex.svg"),
        Source::Claude => include_bytes!("../assets/ai-icons/claude.svg"),
    };
    let mut image = egui_extras::image::load_svg_bytes_with_size(bytes, Some(egui::load::SizeHint::Size(pixels, pixels))).ok()?;
    for pixel in &mut image.pixels {
        *pixel = if source == Source::Dsh {
            Color32::from_rgba_unmultiplied(77, 107, 254, pixel.a())
        } else {
            Color32::from_white_alpha(pixel.a())
        };
    }
    let texture = ctx.load_texture(format!("task-logo-{}-{pixels}", source.key()), image, egui::TextureOptions::LINEAR);
    crate::skin_paint::mark_monochrome(texture.id());
    ctx.data_mut(|data| data.insert_temp(key, texture.clone()));
    Some(texture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_scan::{Source, Stage};

    fn task(stage: Stage, progress: Option<f32>) -> TaskItem {
        TaskItem {
            id: "codex:demo".into(),
            source: Source::Codex,
            title: "写解析器".into(),
            project: "demo".into(),
            model: "gpt-6.2-sol".into(),
            stage,
            progress,
            started_ms: 0,
            updated_ms: 0,
            output_tokens: 0,
            rate: 0.0,
            log: None,
        }
    }

    #[test]
    fn progress_shows_while_alive_and_hides_once_done() {
        let now = 60_000;
        let running = subtitle(&task(Stage::Tool, Some(0.62)), now, false);
        assert!(running.contains("62%"), "{running}");
        // 收场后计划可能还停在 60%：说「已完成 · 62%」会让人以为没做完。
        let done = subtitle(&task(Stage::Done, Some(0.62)), now, false);
        assert!(!done.contains("62%"), "{done}");
    }
}
