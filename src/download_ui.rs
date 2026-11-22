//! 「下载」页：把 Ghost Downloader 最近的下载任务搬上岛（只适配 Ghost）。
//!
//! 任务清单来自 Ghost 自己写的 `tasks.jsonl`，进度与速度走它的本地 WebSocket（`download_scan` 只读，不联网、
//! 不去它们的窗口里点任何东西），所以「读不到」是常态：客户端没装、或者从没下载过
//! 东西，这里就少一行，而不是编一行出来。
use crate::app::*;
use crate::download_scan::{DlState, DownloadItem, ScanConfig};
use crate::extras::{Alert, AlertSource};
use crate::paint::{Card, H};
use crate::task_ui::{BadgeUnit, IslandBadge};
use crate::theme::{self, size, W};
use eframe::egui::{self, Color32, Response};

/// 页面最多显示几行（再多的靠底部「共 N 条」汇总，避免把岛撑爆）。
const ROWS: usize = 4;
/// 行高与首行基线：4 行正好落在标题和底部汇总之间（42..210）。
const ROW_TOP: f32 = 42.0;
const ROW_H: f32 = 42.0;
/// 行尾三个圆形按钮的命中区直径与中心（打开文件 / 打开所在文件夹 / 删除）。
///
/// 右边界必须停在 367（`PW - 52`）以内：`nav` 的左右箭头命中区是
/// `8..34` 和 `385..411`、中心 y 在 `arrow_y = 88`，第二行按钮的高度正好压在上面
/// （删除键原本放在 391，点击会被右箭头抢走，翻页而不是删除）。
const BTN_SIZE: f32 = 24.0;
const BTN_OPEN_X: f32 = 299.0;
const BTN_FOLDER_X: f32 = 327.0;
const BTN_DELETE_X: f32 = 355.0;
/// 文字右边界：按钮命中区从 287 起，留 4 点空隙。
const TEXT_W: f32 = 215.0;
const AMBER: Color32 = Color32::from_rgb(255, 166, 77);
const FAIL: Color32 = Color32::from_rgb(255, 92, 92);
/// 上岛卡片「常驻」用的到期时间：只要任务还在跑，每帧都会把这个值顶回去。
const ISLAND_FOREVER: f64 = 1.0e9;
/// 收场（完成/失败）后卡片再挂多久。
const FINISH_LINGER: f64 = 6.0;
/// 收场超过这么久就不再上岛报（刚开岛时别把一小时前的下载全弹一遍）。
const FINISH_WINDOW_MS: i64 = 6_000;
/// 删除的第二段确认最多等多久。
const CONFIRM_SECONDS: f64 = 4.0;

/// 上岛卡片要画的东西：一条下载的即时快照。
///
/// 存在 `Alert::download` 里，让通知卡片那套排队/让位/超时机制直接为下载服务；
/// 里面只放「画的时候要用」的字段，不持有扫描器状态。
#[derive(Clone, Debug)]
pub(crate) struct DownloadNotice {
    pub id: String,
    pub name: String,
    /// `大小 · 速度 · 45%`，或者完成后的 `大小 · 已完成 · 时间`。
    pub detail: String,
    /// 只有真读到总量时才给百分比——不知道总量就别画一条永远 0% 的条。
    pub percent: Option<f32>,
    pub state: DlState,
}

impl DownloadNotice {
    fn of(item: &DownloadItem, items: &[DownloadItem]) -> Self {
        let others = items
            .iter()
            .filter(|other| other.id != item.id && other.is_active())
            .count();
        // 在跑却一个字节都没记到（实测 Ghost 下载中就是这样）：不画百分比，
        // 0% 不是进度，是谎。拿到真实增量时照常给；没有就画不确定条。
        let percent = if item.total > 0 && !(item.is_active() && item.done == 0) {
            Some(item.percent())
        } else {
            None
        };
        let mut detail = item.size_text();
        if item.is_active() {
            if item.speed > 0 {
                detail.push_str(" · ");
                detail.push_str(&item.speed_text());
            } else if !item.live {
                // 拿不到实时速度时（通道没连上、或只读到 jsonl 存量）：
                // 明说是上次落盘的数字，别让人以为卡住了。
                detail.push_str(" · 上次记录");
            }
            if let Some(progress) = percent {
                detail.push_str(&format!(" · {:.0}%", progress * 100.0));
            }
        } else {
            detail.push_str(" · ");
            detail.push_str(item.state.label());
            detail.push_str(" · ");
            detail.push_str(&item.time_text());
        }
        if others > 0 {
            detail.push_str(&format!(" · 还有 {others} 个在跑"));
        }
        Self { id: item.id.clone(), name: item.name.clone(), detail, percent, state: item.state }
    }
}

/// 状态颜色：跑着的是绿的，等着/停着的是琥珀，失败红、完成深绿。
pub(crate) fn state_color(state: DlState) -> Color32 {
    match state {
        DlState::Downloading => theme::SOFT_GREEN,
        DlState::Paused | DlState::Queued => AMBER,
        DlState::Failed => FAIL,
        DlState::Done => theme::GREEN,
        DlState::Unknown => theme::DIM,
    }
}

fn state_rank(state: DlState) -> u8 {
    match state {
        DlState::Downloading => 0,
        DlState::Paused => 1,
        DlState::Queued => 2,
        DlState::Failed => 3,
        DlState::Unknown => 4,
        DlState::Done => 5,
    }
}

/// 岛上该显示哪一条：正在跑的最急（下载中 > 暂停 > 等待），同档取最近建的；
/// 一条都没在跑时，只报「刚刚收场」的那条几秒，之后安静。
pub(crate) fn pick_island_download<'a>(items: &'a [DownloadItem], now_ms: i64) -> Option<&'a DownloadItem> {
    let mut best: Option<&DownloadItem> = None;
    // 只报真正在写盘的。Paused/Queued 可能是几个月前的残留，
    // 让它们占岛会把通知和常驻入口全顶掉。
    for item in items.iter().filter(|item| item.state == DlState::Downloading) {
        best = Some(match best {
            None => item,
            Some(current) => {
                let better = state_rank(item.state) < state_rank(current.state)
                    || (state_rank(item.state) == state_rank(current.state) && item.created_ms > current.created_ms);
                if better {
                    item
                } else {
                    current
                }
            }
        });
    }
    if best.is_some() {
        return best;
    }
    items
        .iter()
        .filter(|item| item.finished_ms.is_some_and(|at| now_ms.saturating_sub(at) < FINISH_WINDOW_MS))
        .max_by_key(|item| item.finished_ms.unwrap_or(0))
}

/// 一条任务能给岛上画什么角标：字节/秒 + 真实百分比；两样都没有就不画。
pub(crate) fn badge_of(item: &DownloadItem) -> Option<IslandBadge> {
    if !item.is_active() {
        return None;
    }
    let rate = if item.speed > 0 { item.speed as f32 / (1024.0 * 1024.0) } else { 0.0 };
    // 总量未知就别画环：画个假的 0% 比不画更误导。
    let progress = if item.total > 0 && item.done > 0 { Some(item.percent()) } else { None };
    if rate <= 0.0 && progress.is_none() {
        return None;
    }
    Some(IslandBadge { rate, rate_unit: BadgeUnit::Bytes, progress })
}
impl App {
    /// 每帧调用：页面开着、设置分页开着，或者开着「有下载时上岛」时才去读盘。
    ///
    /// 上岛的那一档不能省：`download_island` 要在页面收起时也工作，
    /// 那时页面根本不在前台，扫描不能跟着页面一起停。
    pub(crate) fn download_tick(&mut self) {
        if !self.settings.visible_pages[DOWNLOAD_PAGE] {
            return;
        }
        let watching = (self.open && self.page == DOWNLOAD_PAGE)
            || (self.settings_open && self.settings_tab == 16)
            || self.settings.download_island;
        if !watching {
            return;
        }
        let cfg = ScanConfig {
            // 只适配 Ghost：IDM/FDM 即使旧配置里是开的也不碰（不读注册表、不开 sqlite）。
            sources: [false, false, self.settings.download_sources[2]],
            keep_minutes: self.settings.download_keep_minutes,
            now_ms: crate::download_scan::now_ms(),
        };
        self.download.tick(cfg);
    }

    /// 岛上那张下载卡片（在药丸上，或者还在队列里等着）。
    fn download_card_mut(&mut self, id: &str) -> Option<&mut Alert> {
        let ours = |a: &Alert| a.download.as_ref().is_some_and(|d| d.id == id);
        if self.notice.as_ref().is_some_and(&ours) {
            return self.notice.as_mut();
        }
        self.alerts.iter_mut().find(|a| ours(a))
    }

    /// 每帧调用（页面收起时也走）：把最该显示的那条下载推上岛。
    ///
    /// 进行中：卡片常驻（每帧把 `at` 顶回当前时刻，永远不过期）；
    /// 完成/失败：停止顶 `at`、只留 6 秒，到点由 `tick_alerts` 正常收走。
    pub(crate) fn download_island_tick(&mut self) {
        if !self.settings.visible_pages[DOWNLOAD_PAGE] {
            return;
        }
        if !(self.settings.download_island && self.settings.notifications) {
            // 开关关掉就把卡片撤下来：留一张不再刷新的残影比不显示更糟。
            if self.notice.as_ref().is_some_and(|a| a.download.is_some()) {
                self.notice = None;
            }
            return;
        }
        let now = crate::download_scan::now_ms();
        let items: Vec<DownloadItem> = self.download.items().to_vec();
        // 点过 ✕ 的那条：还在跑就不再自己弹回来；收场后清掉记忆，让「下载完成」还能报一次。
        if let Some(dismissed) = self.download_dismissed.clone() {
            if !items.iter().any(|item| item.id == dismissed && item.is_active()) {
                self.download_dismissed = None;
            }
        }
        let Some(item) = pick_island_download(&items, now) else {
            return;
        };
        if self.download_dismissed.as_deref() == Some(item.id.as_str()) {
            return;
        }
        let active = item.is_active();
        let notice = DownloadNotice::of(item, &items);
        let id = item.id.clone();
        // 先取下来：`download_card_mut` 会可变借用整台 App。
        let now_t = self.t;
        if let Some(alert) = self.download_card_mut(&id) {
            alert.download = Some(notice);
            if active {
                alert.at = now_t;
                alert.duration = ISLAND_FOREVER;
            } else {
                alert.duration = FINISH_LINGER;
            }
        } else {
            let card = Alert {
                source: AlertSource::Internal,
                // 标题给通知列表/系统通知用，岛上的卡片只画文件名与进度。
                title: item.source.label().to_string(),
                body: notice.detail.clone(),
                page: DOWNLOAD_PAGE,
                at: self.t,
                duration: if active { ISLAND_FOREVER } else { FINISH_LINGER },
                bluetooth: None,
                meeting: None,
                unread: None,
                download: Some(notice),
            };
            // 有别的提醒在排队时先让它们走：别拿一张常驻卡把人的消息压住。
            let slot_free = self.notice.is_none() && !self.alerts.iter().any(|a| a.download.is_none());
            if slot_free {
                self.notice = Some(card);
            } else {
                self.alerts.push_back(card);
                while self.alerts.len() > 8 {
                    self.alerts.pop_front();
                }
            }
        }
        // 让位：卡片占着药丸、后面又有真提醒在等，就把它标成已过期——
        // 下一帧 `tick_alerts` 收走它、把提醒顶上来，下载卡片再排回队尾。
        if active && self.alerts.iter().any(|a| a.download.is_none()) {
            if let Some(alert) = self.notice.as_mut().filter(|a| a.download.is_some()) {
                alert.duration = 0.0;
            }
        }
    }

    /// 收起态药丸角标：正在跑的下载（字节/秒 + 真实百分比）。
    ///
    /// 卡片被 ✕ 收起后就是它在显示进度；两者不会同时出现（`badge_quiet` 挡着）。
    pub(crate) fn download_badge(&self) -> Option<IslandBadge> {
        if !self.settings.visible_pages[DOWNLOAD_PAGE] {
            return None;
        }
        let items = self.download.items();
        badge_of(pick_island_download(items, crate::download_scan::now_ms())?)
    }

    /// 岛上只画一枚角标：有下载就先报下载，没有才轮到 AI 任务。
    pub(crate) fn island_badge(&self) -> Option<IslandBadge> {
        self.download_badge().or_else(|| self.task_badge())
    }

    pub(crate) fn page_download(&mut self, c: &Card, _env: &Env, cmds: &mut Vec<Cmd>) {
        let items: Vec<DownloadItem> = self.download.items().to_vec();
        // 标题由页面自己画（`page_title` 对下载页直接 return），和健康页、置物架一致。
        c.text(15.0, 25.0, "最近", size::TITLE, W::Bold, theme::WHITE, H::Left);
        let width = c.text_w("最近", size::TITLE, W::Bold);
        c.text(15.0 + width + 1.5, 25.0, "下载", size::TITLE, W::Bold, theme::CYAN, H::Left);
        if items.is_empty() {
            let all_off = !self.settings.download_sources[2];
            let (title, hint) = if all_off {
                (
                    "Ghost Downloader 还关着",
                    "到 设置 → 下载 里把它打开，这里才会去读它的下载记录",
                )
            } else {
                (
                    "没有最近的下载",
                    "Ghost Downloader 下过东西以后，这里会列出来；没装或从没下载过就一直是空的",
                )
            };
            c.text(34.0, 70.0, title, 13.0, W::Bold, theme::WHITE, H::Left);
            let hint = c.fit(hint, 11.0, W::Regular, PW - 96.0);
            c.text(34.0, 91.0, &hint, 11.0, W::Regular, theme::GRAY, H::Left);
            // 后台扫描是异步的：空列表时慢一点重绘，等结果回来。
            c.ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            return;
        }
        let active = items.iter().filter(|item| item.is_active()).count();
        // 第二段确认会过期：过时就别再显示「确认删?」了。
        if self.download_confirm.as_ref().is_some_and(|(_, at)| self.t - *at > CONFIRM_SECONDS) {
            self.download_confirm = None;
        }
        // 页面里的速度是核心信息，不受「上岛显示速度」那个开关影响。
            let show_speed = true;
        for (index, item) in items.iter().take(ROWS).enumerate() {
            let y = ROW_TOP + index as f32 * ROW_H;
            let exists = item.path.is_file();
            // 行背景（左半段）：按钮有自己的命中区，这里不覆盖它们。
            let row = c.hit(40.0, y - 6.0, 244.0, ROW_H - 4.0, ("dl-row", item.id.as_str()));
            let (hover, _) = c.hover_t(&row);
            if hover > 0.01 {
                c.fill(40.0, y - 6.0, PW - 52.0, ROW_H - 4.0, 8.0, Color32::from_white_alpha((hover * 10.0) as u8));
            }
            let tint = state_color(item.state);
            crate::paint::tray(c, 52.0, y + 9.0, 14.0, 0.0, tint, Color32::from_white_alpha(26));
            let title = c.fit(&item.name, 12.0, W::Semibold, TEXT_W);
            c.text(68.0, y + 10.0, &title, 12.0, W::Semibold, theme::WHITE, H::Left);
            let mut sub = item.subtitle(show_speed);
            if item.state == DlState::Done && !exists {
                sub.push_str(" · 文件已不在");
            }
            if item.is_active() && !item.live {
                // 客户端没在跑（或库内没有速度字段）：这份进度是上次落盘的数字。
                sub.push_str(" · 上次记录");
            }
            let sub = c.fit(&sub, 10.0, W::Regular, TEXT_W);
            c.text(68.0, y + 25.0, &sub, 10.0, W::Regular, theme::GRAY, H::Left);
            if item.is_active() && item.total > 0 {
                c.fill(68.0, y + 32.0, TEXT_W, 2.5, 1.25, Color32::from_white_alpha(26));
                if item.done > 0 {
                    let progress = item.percent().clamp(0.0, 1.0);
                    c.fill(68.0, y + 32.0, (TEXT_W * progress).max(2.5), 2.5, 1.25, tint);
                } else {
                    // 在跑但还没记到字节（Ghost 就是这样）：跑一段来回的条，不伪造 0%。
                    let w = TEXT_W * 0.32;
                    let phase = ((self.t * 0.55) % 1.0) as f32;
                    c.fill(68.0 + (TEXT_W - w) * phase, y + 32.0, w, 2.5, 1.25, tint.linear_multiply(0.55));
                }
            }
            // 点行本体 = 打开这个文件。
            let tip = item.path.display().to_string();
            if row.clone().on_hover_text(&tip).clicked() && exists {
                cmds.push(Cmd::Open(item.path.clone()));
            }
            // 打开文件
            if glyph_button(c, BTN_OPEN_X, y + 6.0, ("dl-open", item.id.as_str()), exists, theme::GRAY).clicked()
                && exists
            {
                cmds.push(Cmd::Open(item.path.clone()));
            }
            // 打开所在文件夹：文件没了也能定位（目录可能还在）。
            if glyph_button(c, BTN_FOLDER_X, y + 6.0, ("dl-folder", item.id.as_str()), true, theme::GRAY).clicked() {
                cmds.push(Cmd::Reveal(item.path.clone()));
            }
            // 删除：只有「已完成且文件还在」的行可点，第二段确认才真删。
            let deletable = item.state == DlState::Done && exists;
            let confirming = self
                .download_confirm
                .as_ref()
                .is_some_and(|(id, _)| id == &item.id);
            if confirming {
                let (hit, inner) = crate::paint::capsule(
                    c,
                    BTN_DELETE_X - 22.0,
                    y + 5.0,
                    44.0,
                    22.0,
                    &format!("dl-confirm-{}", item.id),
                    theme::RED,
                    theme::RED,
                );
                inner.centered_text(BTN_DELETE_X, y + 16.0, "确认删?", 9.5, W::Semibold, theme::WHITE, H::Center);
                if hit.clicked() {
                    self.download_confirm = None;
                    cmds.push(Cmd::DownloadDelete(item.path.clone()));
                }
            } else {
                let hit = glyph_button(c, BTN_DELETE_X, y + 6.0, ("dl-delete", item.id.as_str()), deletable, FAIL);

                if !deletable {
                    // 说清为什么不能删，免得以为按钮坏了。
                    let why = if exists { "只有已完成的下载能从岛上删" } else { "文件已经不在了" };
                    hit.clone().on_hover_text(why);
                } else if hit.clicked() {
                    if self.settings.download_confirm_delete {
                        self.download_confirm = Some((item.id.clone(), self.t));
                    } else {
                        cmds.push(Cmd::DownloadDelete(item.path.clone()));
                    }
                }
            }
        }
        c.text(
            15.0,
            DOWNLOAD_H - 16.0,
            &format!("共 {} 条 · 进行中 {}", items.len(), active),
            10.0,
            W::Regular,
            theme::DIM,
            H::Left,
        );
        // 有任务还在跑时，进度和速度要继续走。
        if active > 0 {
            c.ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    pub(crate) fn download_settings(&mut self, ui: &mut egui::Ui) {
        use crate::extras::{setting_row, settings_card, settings_label, settings_segmented};
        settings_label(ui, "下载来源");
        settings_card(ui, |ui| {
            setting_row(
                ui,
                "Ghost Downloader",
                "读取任务的进度、速度与完成时间",
                &mut self.settings.download_sources[2],
            );
        });
        settings_label(ui, "显示");
        settings_card(ui, |ui| {
            settings_segmented(
                ui,
                &mut self.settings.download_keep_minutes,
                &[(60u32, "1 小时"), (360, "6 小时"), (1440, "1 天"), (10080, "7 天")],
                74.0,
            );
            ui.label(
                egui::RichText::new("只显示这段时间内创建或完成的下载；更早的任务不再占位。")
                    .size(11.5)
                    .color(egui::Color32::from_gray(150)),
            );
            ui.separator();
            setting_row(
                ui,
                "有下载时上岛",
                "有新任务开始时岛上直接显示文件名与进度",
                &mut self.settings.download_island,
            );
            ui.separator();
            setting_row(
                ui,
                "上岛显示速度",
                "岛上同时显示下载速度（MB/s）",
                &mut self.settings.download_island_speed,
            );
            ui.separator();
            setting_row(
                ui,
                "删除前确认",
                "删文件前问一句；确认后送进回收站，还能撤回来",
                &mut self.settings.download_confirm_delete,
            );
        });
        settings_label(ui, "说明");
        settings_card(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "岛仔只读 Ghost Downloader 自己写在磁盘上的记录，不联网、不上传，\
                     也不去它们的界面里点任何东西。\
                     「删除」只是把文件送进回收站，记录本身不动；文件已经被移走的行会标成读不到。",
                )
                .size(11.5)
                .color(egui::Color32::from_gray(155)),
            );
        });
    }
}

/// 行尾那个圆形小按钮：命中区 + 悬停圆底，返回响应（glyph 由调用方画）。
fn glyph_button(c: &Card, cx: f32, cy: f32, id: (&str, &str), enabled: bool, tint: Color32) -> Response {
    let r = c.hit(cx - BTN_SIZE / 2.0, cy - BTN_SIZE / 2.0, BTN_SIZE, BTN_SIZE, id);
    let (hover, _) = c.hover_t(&r);
    if hover > 0.01 {
        c.circle(cx, cy, BTN_SIZE / 2.0, Color32::from_white_alpha((hover * 20.0) as u8));
    }
    let ink = if enabled { tint } else { Color32::from_white_alpha(38) };
    match id.0 {
        "dl-open" => c.poly(&[(cx - 3.0, cy - 4.5), (cx - 3.0, cy + 4.5), (cx + 4.5, cy)], ink),
        "dl-folder" => {
            c.fill(cx - 6.5, cy - 4.5, 5.0, 2.5, 1.0, ink);
            c.fill(cx - 6.5, cy - 2.5, 13.0, 8.0, 1.6, ink);
        }
        _ => crate::paint::close(c, cx, cy, 4.0, ink),
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download_scan::Source;
    use std::path::PathBuf;

    fn item(id: &str, state: DlState, created_ms: i64, finished_ms: Option<i64>) -> DownloadItem {
        DownloadItem {
            id: id.into(),
            source: Source::Ghost,
            name: format!("{id}.bin"),
            path: PathBuf::from(format!("C:/none/{id}.bin")),
            total: 1000,
            done: 500,
            speed: 2 * 1024 * 1024,
            state,
            created_ms,
            finished_ms,
            detail: None,
            live: true,
        }
    }

    /// 岛上只报一条：正在跑的优先，跑的里面先报「下载中」再报暂停/排队。
    #[test]
    fn island_prefers_the_running_download_over_a_just_finished_one() {
        let now = 1_000_000;
        let items = vec![
            item("ghost:done", DlState::Done, now - 90_000, Some(now - 3_000)),
            item("ghost:paused", DlState::Paused, now - 30_000, None),
            item("ghost:live", DlState::Downloading, now - 60_000, None),
        ];
        let picked = pick_island_download(&items, now).expect("有正在跑的就得报");
        assert_eq!(picked.id, "ghost:live", "下载中压过暂停");
    }

    /// 同一档状态里报最近开始的那个：多任务时「最近活跃」比「最早建的」更有用。
    #[test]
    fn island_reports_the_newest_of_two_running_downloads() {
        let now = 1_000_000;
        let items = vec![
            item("ghost:old", DlState::Downloading, now - 600_000, None),
            item("ghost:new", DlState::Downloading, now - 5_000, None),
        ];
        assert_eq!(pick_island_download(&items, now).map(|i| i.id.as_str()), Some("ghost:new"));
        let flipped: Vec<DownloadItem> = items.into_iter().rev().collect();
        assert_eq!(pick_island_download(&flipped, now).map(|i| i.id.as_str()), Some("ghost:new"), "顺序不该影响结果");
    }

    /// 全部收场以后只在 6 秒内报一次，过期就不再占岛。
    #[test]
    fn finished_downloads_only_linger_for_six_seconds() {
        let now = 1_000_000;
        let fresh = vec![item("ghost:fresh", DlState::Done, now - 60_000, Some(now - 2_000))];
        assert!(pick_island_download(&fresh, now).is_some(), "刚完成要报一次");
        let stale = vec![item("ghost:stale", DlState::Done, now - 60_000, Some(now - 10_000))];
        assert!(pick_island_download(&stale, now).is_none(), "十分钟前的完成不该再占岛");
        let failed = vec![item("ghost:failed", DlState::Failed, now - 60_000, Some(now - 1_000))];
        assert!(pick_island_download(&failed, now).is_some(), "失败也要报一次");
        assert!(pick_island_download(&[], now).is_none());
    }

    /// 卡片副行：大小、速度、百分比都要在；还有别的在跑时补一句数量。
    #[test]
    fn notice_detail_carries_size_speed_percent_and_the_rest_count() {
        let now = 1_000_000;
        let live = item("ghost:live", DlState::Downloading, now - 60_000, None);
        let other = item("ghost:other", DlState::Downloading, now - 60_000, None);
        let only = [live.clone()];
        let notice = DownloadNotice::of(&live, &only);
        assert_eq!(notice.id, "ghost:live");
        assert_eq!(notice.name, "ghost:live.bin");
        assert!(notice.detail.contains("500 B / 1000 B"), "副行要有大小：{}", notice.detail);
        assert!(notice.detail.contains("2.00 MB/s"), "副行要有速度：{}", notice.detail);
        assert!(notice.detail.contains("50%"), "副行要有百分比：{}", notice.detail);
        assert!(!notice.detail.contains("还有"), "只有一条时不该提别的任务：{}", notice.detail);
        assert!((notice.percent.unwrap() - 0.5).abs() < 0.001);
        let both = [live.clone(), other];
        let notice = DownloadNotice::of(&live, &both);
        assert!(notice.detail.contains("还有 1 个在跑"), "多任务要数给用户看：{}", notice.detail);
        // 大小未知时宁可不画环，也不画个假的 0%。
        let mut unknown = live.clone();
        unknown.total = 0;
        assert!(DownloadNotice::of(&unknown, &[unknown.clone()]).percent.is_none());
    }

    /// 收场的卡片写「已完成/失败」和时间，不再写速度。
    #[test]
    fn finished_notice_shows_state_and_time_instead_of_speed() {
        let now = 1_000_000;
        let done = item("ghost:done", DlState::Done, now - 60_000, Some(now - 2_000));
        let notice = DownloadNotice::of(&done, &[done.clone()]);
        assert!(notice.detail.contains("已完成"), "{}", notice.detail);
        assert!(!notice.detail.contains("/s"), "收场不该再报速度：{}", notice.detail);
        assert!(notice.detail.contains("20:0") || notice.detail.contains(":"), "要有时间：{}", notice.detail);
    }

    /// 颜色能区分「在跑 / 暂停 / 失败 / 完成」，别都一个色。
    #[test]
    fn running_without_a_single_byte_gets_no_fake_percentage() {
        // 实测 Ghost 下载中就是这样：状态是「下载中」，但一个字节都没记到。
        let mut item = item("ghost:run", DlState::Downloading, 1_000_000, None);
        item.total = 400 * 1024 * 1024;
        item.done = 0;
        item.speed = 0;
        let notice = DownloadNotice::of(&item, std::slice::from_ref(&item));
        assert!(notice.percent.is_none(), "0% 不叫进度");
        assert!(badge_of(&item).is_none(), "没有百分比也没有速度，就别画角标");
        // 一旦真有增量，就照常给百分比。
        item.done = 100 * 1024 * 1024;
        let notice = DownloadNotice::of(&item, std::slice::from_ref(&item));
        assert!((notice.percent.unwrap() - 0.25).abs() < 0.01);
    }

    #[test]
    fn state_color_separates_running_failed_and_done() {
        assert_eq!(state_color(DlState::Downloading), theme::SOFT_GREEN);
        assert_eq!(state_color(DlState::Paused), AMBER);
        assert_eq!(state_color(DlState::Queued), AMBER);
        assert_eq!(state_color(DlState::Failed), FAIL);
        assert_eq!(state_color(DlState::Done), theme::GREEN);
        assert_eq!(state_color(DlState::Unknown), theme::DIM);
        assert_ne!(state_color(DlState::Downloading), state_color(DlState::Done));
    }
}