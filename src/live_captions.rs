//! Windows「实时辅助字幕」(LiveCaptions.exe) 的 UI Automation 文本读取。
//!
//! 为什么这样实现：
//! - Live Captions 没有公开的文本 API，唯一稳定的文本出口是它的 UI Automation 树，
//!   所以这里只走「找顶层窗口 → 找 CaptionsTextBlock → 读 CurrentName」这一条最窄链路。
//! - COM 的 apartment 是线程绑定的：`IUIAutomation` 必须在真正使用它的线程里创建。
//!   因此轮询线程自己 `CoInitializeEx(MTA)` 并创建 UIA 对象，调用方（egui 主线程，通常是 STA）
//!   完全不接触 COM，避免把主线程拖进 MTA，也避免跨 apartment 传递接口指针。
//! - 轮询线程同时兼做进程看护：用户关掉 LiveCaptions 后 2 秒自动重拉，字幕才能长期可用。
//! - 隐藏采用「最小化 + WS_EX_TOOLWINDOW」而不是 SW_HIDE：SW_HIDE 会让部分 UWP/WinUI 宿主
//!   停止排版，UIA 文本可能随之不再更新；最小化能保持它在后台继续转写。
//! - 恢复窗口的逻辑同时挂在 `stop()` 和 `Drop` 上：无论正常收尾还是中途丢弃，
//!   都不会把 LiveCaptions 留在「任务栏看不见、又没法恢复」的状态。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

#[cfg(windows)]
use std::process::{Child, Command};
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use windows::core::VARIANT;
#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, HWND, RECT, RPC_E_CHANGED_MODE};
#[cfg(windows)]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
#[cfg(windows)]
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE};
#[cfg(windows)]
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Children, TreeScope_Descendants,
    UIA_AutomationIdPropertyId, UIA_ProcessIdPropertyId, UIA_E_ELEMENTNOTAVAILABLE,
};
#[cfg(windows)]
use windows::Win32::Foundation::{BOOL, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, SetWindowDisplayAffinity,
    GetWindowLongW, SetLayeredWindowAttributes, SetWindowLongW, SetWindowPos, ShowWindow, GWL_EXSTYLE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SW_RESTORE, SW_SHOWNOACTIVATE, LWA_ALPHA, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
};

#[cfg(windows)]
const LIVE_CAPTIONS_EXE: &str = "LiveCaptions.exe";
#[cfg(windows)]
const CAPTIONS_AUTOMATION_ID: &str = "CaptionsTextBlock";
/// 轮询间隔：120ms 足够跟上字幕滚动，又不会把 UIA 调用打成忙等。
#[cfg(windows)]
const POLL_INTERVAL_MS: u64 = 120;
/// 进程退出后的重启延迟，避免崩溃循环里疯狂拉起进程。
#[cfg(windows)]
const RESTART_DELAY: Duration = Duration::from_secs(2);

/// 一条分句结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sentence {
    pub text: String,
    /// 是否以句末标点结尾：true = 这句已经说完；false = 还在增长中的末尾残留。
    pub final_: bool,
}

/// 轮询线程推给调用方的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    /// 当前滚动字幕窗口的完整文本快照（连续空白已归一化）。
    ///
    /// 注意：Live Captions 的控件只保留最近几行，文本会滚动、旧行会消失，
    /// 所以这里给的是快照而不是增量；需要按句展示时交给 [`split_sentences`]。
    Text(String),
    /// 可恢复的错误：启动失败、COM/UIA 创建失败、读取字幕失败等。
    Error(String),
    /// 首次成功读到字幕控件（可能文本还是空的），可以开始消费 Text 了。
    Ready,
}

/// 实时字幕读取器。`start()` 后由后台线程轮询，`try_recv()` 非阻塞取事件。
pub struct LiveCaptions {
    rx: Receiver<LiveEvent>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// 当前 Live Captions 顶层窗口句柄（0 = 还没找到）；`show_window()` 用。
    #[cfg_attr(not(windows), allow(dead_code))]
    hwnd: Arc<AtomicIsize>,
}

impl LiveCaptions {
    /// 启动（或复用）LiveCaptions.exe 并开始轮询。
    ///
    /// 只有「启动进程」这一瞬间的失败会通过 `Err` 返回；之后的窗口查找/UIA 错误
    /// 走 [`LiveEvent::Error`]，线程会持续自愈，不会因为一次失败就退出。
    #[cfg(windows)]
    pub fn start() -> Result<LiveCaptions, String> {
        let (pid, child) = match find_live_captions_pid() {
            // 已有实例就复用，避免拉起第二个 LiveCaptions 抢麦克风；
            // 上一次会话可能把它挪到了屏幕外，复用时先放回屏幕内，等出字后再挪走。
            Some(pid) => {
                unpark_pid_windows(pid);
                (pid, None)
            }
            None => {
                let child = Command::new(LIVE_CAPTIONS_EXE)
                    .spawn()
                    .map_err(|err| format!("启动 LiveCaptions 失败：{err}"))?;
                let pid = child.id();
                // 抢在窗口绘制前按掉：Live Captions 是 WinUI 应用，窗口晚于进程出现。
                for _ in 0..50 {
                    hide_pid_windows(pid);
                    std::thread::sleep(Duration::from_millis(30));
                }
                (pid, Some(child))
            }
        };

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let hwnd = Arc::new(AtomicIsize::new(0));
        let thread_stop = Arc::clone(&stop);
        let thread_hwnd = Arc::clone(&hwnd);
        let join = thread::Builder::new()
            .name("live-captions-uia".to_string())
            .spawn(move || run_worker(tx, thread_stop, thread_hwnd, pid, child))
            .map_err(|err| format!("启动字幕轮询线程失败：{err}"))?;

        Ok(LiveCaptions {
            rx,
            stop,
            join: Some(join),
            hwnd,
        })
    }

    #[cfg(not(windows))]
    pub fn start() -> Result<LiveCaptions, String> {
        Err("仅支持 Windows".to_string())
    }

    /// 非阻塞取一个事件；通道为空或线程已退出时返回 None。
    pub fn try_recv(&self) -> Option<LiveEvent> {
        match self.rx.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    /// 停止轮询并恢复 LiveCaptions 窗口。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // 停止字幕时不要把 LiveCaptions 窗口放出来：它只是取字用的后台进程。
        // 恢复窗口只留给用户显式点「显示系统窗口」（show_window）。
        // 即使这个实例不是我们拉起的（复用了系统已在跑的 LiveCaptions），
        // 也要在停止后按回透明，否则它会一直停在屏幕上。
        // 用户点「停止」时要求连窗口一起消失：LiveCaptions 只是取字的后台进程，
        // 留着它只会继续占麦克风、继续在屏幕上闪。复用的系统实例也一起结束。
        kill_all_live_captions();
        self.hwnd.store(0, Ordering::SeqCst);
    }

    /// 取消隐藏：去掉 WS_EX_TOOLWINDOW 并 SW_RESTORE。
    ///
    /// 只恢复窗口，不停止轮询；之后轮询线程不会再次把它压回最小化
    /// （隐藏只在发现「新窗口」时执行一次）。
    pub fn show_window(&self) {
        #[cfg(windows)]
        {
            let raw = self.hwnd.load(Ordering::SeqCst);
            if raw != 0 {
                unsafe { restore_window(HWND(raw as *mut core::ffi::c_void)) };
            }
        }
    }
}

impl Drop for LiveCaptions {
    fn drop(&mut self) {
        // 调用方忘记 stop() 时也不能把后台线程和隐藏窗口漏掉。
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// 分句（纯函数，跨平台，可单测）
// ---------------------------------------------------------------------------

/// 把一段字幕文本切成句子。
///
/// 规则：
/// - 句末标点：`.?!。？！…`；连续标点（`?!`、`……`）归入同一句；
/// - 以句末标点结尾的句子 `final_ = true`；
/// - 末尾没有标点的残留也返回，`final_ = false`（调用方可以等它继续长）；
/// - 连续重复的句子只保留一条；所有连续空白归一化为一个空格；
/// - 不会产生空句（纯空白/纯标点输入最多得到一条非空结果）。
pub fn split_sentences(text: &str) -> Vec<Sentence> {
    let normalized = normalize_whitespace(text);
    let mut out: Vec<Sentence> = Vec::new();
    let mut current = String::new();
    let mut chars = normalized.chars().peekable();

    while let Some(ch) = chars.next() {
        current.push(ch);
        if !is_sentence_end(ch) {
            continue;
        }

        // "?!", "……" 这类连续标点应该留在同一句里，否则会在标点之间切出空句。
        while let Some(&next) = chars.peek() {
            if is_sentence_end(next) {
                current.push(next);
                chars.next();
            } else {
                break;
            }
        }
        // 右引号/右括号跟在句末标点后，仍属于这一句。
        while let Some(&next) = chars.peek() {
            if is_closing(next) {
                current.push(next);
                chars.next();
            } else {
                break;
            }
        }

        push_sentence(&mut out, &mut current, true);
    }

    // 末尾没有标点的部分：仍然返回，但标记为非 final。
    push_sentence(&mut out, &mut current, false);
    out
}

fn push_sentence(out: &mut Vec<Sentence>, current: &mut String, final_: bool) {
    let text = normalize_whitespace(current);
    current.clear();
    if text.is_empty() {
        return;
    }
    if out.last().map(|last| last.text == text).unwrap_or(false) {
        return;
    }
    out.push(Sentence { text, final_ });
}

fn is_sentence_end(ch: char) -> bool {
    matches!(ch, '.' | '?' | '!' | '。' | '？' | '！' | '…')
}

fn is_closing(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | '”' | '’' | '）' | ')' | '】' | ']' | '」' | '』' | '》' | '〉'
    )
}

fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Windows 实现
// ---------------------------------------------------------------------------

/// 轮询线程的状态机。所有 UIA 对象都只在这个线程里使用。
#[cfg(windows)]
struct Worker {
    tx: mpsc::Sender<LiveEvent>,
    stop: Arc<AtomicBool>,
    hwnd_slot: Arc<AtomicIsize>,
    automation: IUIAutomation,
    pid: u32,
    child: Option<Child>,
    window: Option<IUIAutomationElement>,
    caption: Option<IUIAutomationElement>,
    /// 已经隐藏过的 HWND：只在换窗口时隐藏一次，避免 show_window() 后被反复压回去。
    hidden_hwnd: isize,
    last_text: String,
    ready_sent: bool,
    last_error: String,
    next_launch: Instant,
}

#[cfg(windows)]
impl Drop for Worker {
    fn drop(&mut self) {
        // 停止识别后别把 LiveCaptions 留在后台；只结束我们自己拉起的实例。
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(windows)]
fn run_worker(
    tx: mpsc::Sender<LiveEvent>,
    stop: Arc<AtomicBool>,
    hwnd_slot: Arc<AtomicIsize>,
    pid: u32,
    child: Option<Child>,
) {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // RPC_E_CHANGED_MODE 表示这个线程早就是 STA 了（被宿主初始化过）；COM 已可用，
    // 继续用 UIA 没问题，但不能配对调用 CoUninitialize（那不是我们初始化的）。
    let should_uninitialize = if hr.is_ok() {
        true
    } else if hr == RPC_E_CHANGED_MODE {
        false
    } else {
        let _ = tx.send(LiveEvent::Error(format!("初始化 COM 失败：{hr:?}")));
        return;
    };

    let automation: IUIAutomation =
        match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) } {
            Ok(automation) => automation,
            Err(err) => {
                let _ = tx.send(LiveEvent::Error(format!("创建 UI Automation 失败：{err}")));
                if should_uninitialize {
                    unsafe { CoUninitialize() };
                }
                return;
            }
        };

    let mut worker = Worker {
        tx,
        stop,
        hwnd_slot,
        automation,
        pid,
        child,
        window: None,
        caption: None,
        hidden_hwnd: 0,
        last_text: String::new(),
        ready_sent: false,
        last_error: String::new(),
        next_launch: Instant::now(),
    };

    while !worker.stop.load(Ordering::Relaxed) {
        if !worker.step() {
            break;
        }
        // 分片睡眠：stop() 最多等 10ms，而不是被整整 120ms 的轮询拖住。
        for _ in 0..(POLL_INTERVAL_MS / 10) {
            if worker.stop.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    worker.restore_current_window();
    if should_uninitialize {
        unsafe { CoUninitialize() };
    }
}

#[cfg(windows)]
impl Worker {
    /// 跑一轮状态机；返回 false 表示接收端已经没了，线程应当退出。
    fn step(&mut self) -> bool {
        // 1. 进程看护：退出后等 2 秒再拉，防止崩溃循环。
        if self.pid != 0 && !self.process_alive() {
            self.forget_process();
            self.next_launch = Instant::now() + RESTART_DELAY;
            self.report("LiveCaptions 进程已退出，2 秒后重试".to_string());
        }

        // 2. 没有进程：先看用户是否自己开了一个，没有再拉起。
        if self.pid == 0 {
            if Instant::now() < self.next_launch {
                return true;
            }
            match find_live_captions_pid() {
                Some(pid) => {
                    self.pid = pid;
                    self.child = None;
                    self.ready_sent = false;
                }
                None => match Command::new(LIVE_CAPTIONS_EXE).spawn() {
                    Ok(child) => {
                        self.pid = child.id();
                        self.child = Some(child);
                        self.ready_sent = false;
                    }
                    Err(err) => {
                        self.next_launch = Instant::now() + RESTART_DELAY;
                        self.report(format!("启动 LiveCaptions 失败：{err}"));
                        return true;
                    }
                },
            }
        }

        // 3. 找顶层窗口（root 的直接子元素，按进程号过滤）。
        //    Live Captions 可能被系统重启成新进程/新窗口，每轮把所有实例压透明。
        hide_all_live_captions();
        if self.window.is_none() {
            self.try_find_window();
            if self.window.is_none() {
                return true;
            }
        }
        // 初始化完成后再挪出屏幕：此时 LiveCaptions 已经在识别，挪走不会中断 UIA 更新。
        if self.caption.is_some() && self.hidden_hwnd != 0 {
            unsafe { SetWindowPos(HWND(self.hidden_hwnd as *mut core::ffi::c_void), None, -12000, -12000, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER); }
        }

        // 4. 找 CaptionsTextBlock。找不到就每轮重试，不做失败计数——
        //    LiveCaptions 启动慢、切主题重建控件都可能短暂找不到。
        if self.caption.is_none() {
            let found = {
                let window = self.window.as_ref().expect("window 已在上面确认存在");
                find_caption(&self.automation, window)
            };
            match found {
                Some(caption) => self.caption = Some(caption),
                None => {
                    // 窗口元素本身失效时把 window 也清掉，下一轮重新从 root 找。
                    let window_gone = self
                        .window
                        .as_ref()
                        .map(|window| unsafe { window.CurrentNativeWindowHandle().is_err() })
                        .unwrap_or(true);
                    if window_gone {
                        self.window = None;
                        self.caption = None;
                    }
                    return true;
                }
            }
        }

        // 5. 读整段滚动字幕。文本没变就不推送，避免把 120ms 轮询变成事件洪水。
        let result = {
            let caption = self.caption.as_ref().expect("caption 已在上面确认存在");
            unsafe { caption.CurrentName() }
        };
        match result {
            Ok(name) => {
                let text = normalize_whitespace(&String::from_utf16_lossy(name.as_wide()));
                if text != self.last_text {
                    self.last_text = text.clone();
                    if !text.is_empty() && !self.emit(LiveEvent::Text(text)) {
                        return false;
                    }
                }
                if !self.ready_sent {
                    self.ready_sent = true;
                    if !self.emit(LiveEvent::Ready) {
                        return false;
                    }
                }
                self.last_error.clear();
            }
            Err(err) => {
                // ElementNotAvailable 是控件被重建/窗口切页的正常现象，静默重找；
                // 其它错误才上报，但仍保留元素为 None 以便下一轮重试。
                self.caption = None;
                if !is_element_not_available(&err) {
                    self.report(format!("读取实时字幕失败：{err}"));
                }
            }
        }

        true
    }

    fn try_find_window(&mut self) {
        let Some(window) = find_window(&self.automation, self.pid) else {
            return;
        };

        // 窗口第一次出现时隐藏它；hidden_hwnd 记录后不再重复最小化。
        unsafe {
            if let Ok(hwnd) = window.CurrentNativeWindowHandle() {
                let raw = hwnd.0 as isize;
                if raw != 0 {
                    // 用户可能用 Win+Ctrl+L 把窗口重新显示出来（hwnd 不变），
                    // 所以每次轮询都压回透明，不能只在第一次隐藏。
                    hide_window(hwnd);
                    self.hidden_hwnd = raw;
                    self.hwnd_slot.store(raw, Ordering::SeqCst);
                }
            }
        }

        self.window = Some(window);
        self.caption = None;
    }

    fn process_alive(&mut self) -> bool {
        if let Some(child) = self.child.as_mut() {
            // 自己拉起的进程：try_wait 最准，也不会被权限问题干扰。
            match child.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                // 状态未知时按存活处理，宁可少拉一次也不要拉起第二个实例。
                Err(_) => true,
            }
        } else {
            is_process_alive(self.pid)
        }
    }

    fn forget_process(&mut self) {
        self.pid = 0;
        self.child = None;
        self.window = None;
        self.caption = None;
        self.hidden_hwnd = 0;
        self.last_text.clear();
        self.ready_sent = false;
        self.hwnd_slot.store(0, Ordering::SeqCst);
    }

    fn emit(&self, event: LiveEvent) -> bool {
        self.tx.send(event).is_ok()
    }

    fn report(&mut self, message: String) {
        // 同一个错误只报一次，避免 2 秒重启循环把 Error 事件刷屏。
        if self.last_error != message {
            self.last_error = message;
            let _ = self.tx.send(LiveEvent::Error(self.last_error.clone()));
        }
    }

    fn restore_current_window(&mut self) {
        let raw = self.hwnd_slot.swap(0, Ordering::SeqCst);
        if raw != 0 {
            unsafe { restore_window(HWND(raw as *mut core::ffi::c_void)) };
        }
        self.hidden_hwnd = 0;
    }
}

// ---------------------------------------------------------------------------
// Win32 / UIA 小工具
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn find_window(automation: &IUIAutomation, pid: u32) -> Option<IUIAutomationElement> {
    unsafe {
        let root = automation.GetRootElement().ok()?;
        // 顶层窗口是桌面根节点的直接子元素；用进程号而不是窗口标题，
        // 避免系统语言不同导致 "Live Captions" 标题匹配不上。
        let condition = automation
            .CreatePropertyCondition(UIA_ProcessIdPropertyId, &VARIANT::from(pid as i32))
            .ok()?;
        root.FindFirst(TreeScope_Children, &condition).ok()
    }
}

#[cfg(windows)]
fn find_caption(
    automation: &IUIAutomation,
    window: &IUIAutomationElement,
) -> Option<IUIAutomationElement> {
    unsafe {
        let condition = automation
            .CreatePropertyCondition(
                UIA_AutomationIdPropertyId,
                &VARIANT::from(CAPTIONS_AUTOMATION_ID),
            )
            .ok()?;
        window.FindFirst(TreeScope_Descendants, &condition).ok()
    }
}

#[cfg(windows)]
fn is_element_not_available(err: &windows::core::Error) -> bool {
    err.code().0 as u32 == UIA_E_ELEMENTNOTAVAILABLE
}

/// 枚举所有进程，按 exe 文件名找 LiveCaptions。用 ToolHelp 而不是 UIA，
/// 因为启动前还没有窗口可查，只能先确认进程是否已存在。
#[cfg(windows)]
fn find_live_captions_pid() -> Option<u32> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut found = None;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let exe = String::from_utf16_lossy(&entry.szExeFile);
                if exe
                    .trim_end_matches('\0')
                    .eq_ignore_ascii_case(LIVE_CAPTIONS_EXE)
                {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

/// 只查询存活状态，不碰进程内部。OpenProcess 失败基本意味着进程已经没了；
/// LiveCaptions 不是受保护进程，不存在持续的 ACCESS_DENIED。
#[cfg(windows)]
fn is_process_alive(pid: u32) -> bool {
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

/// 完全透明 + 工具窗口样式：窗口不最小化、不占任务栏、不进 Alt-Tab，也不会在屏幕角落露出来。
/// 用分层窗口把 alpha 降到 0，窗口仍在正常渲染，UI Automation 的文本才会持续更新；
/// 最小化在 Win11 上会把它缩成左下角一块可见的卡片，用户能直接拉出来。
#[cfg(windows)]
/// 把指定进程的所有可见顶层窗口压成透明工具窗。
///
/// 只在 UIA 找到 CaptionsTextBlock 之后才隐藏是不够的：Live Captions 的窗口
/// 通常比 UIA 元素先出现，用户会看到它闪一下甚至停留。这里按 PID 枚举窗口，
/// 启动后 1.5 秒内每 30ms 扫一次，尽量在它画出来之前就按掉。
#[cfg(windows)]
fn hide_all_live_captions() {
    unsafe extern "system" fn visit(hwnd: HWND, _param: LPARAM) -> BOOL {
        if !IsWindowVisible(hwnd).as_bool() { return BOOL(1); }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 && process_is_live_captions(pid) { hide_window(hwnd); }
        BOOL(1)
    }
    unsafe { let _ = EnumWindows(Some(visit), LPARAM(0)); }
}

/// 系统可能把 Live Captions 重新拉到另一个进程里（切换语言/主题时会重建窗口），
/// 只盯着记录的 pid 会漏掉新窗口。这里按进程可执行文件名判断，任何实例都压透明。
#[cfg(windows)]
fn process_is_live_captions(pid: u32) -> bool {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false; };
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        ok && String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase().ends_with("livecaptions.exe")
    }
}

#[cfg(windows)]
fn unpark_pid_windows(pid: u32) {
    unsafe {
        let _ = EnumWindows(Some(unpark_callback), LPARAM(pid as isize));
    }
}

unsafe extern "system" fn unpark_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let pid = lparam.0 as u32;
    let mut owner = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
    if owner != pid || !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return true.into();
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() && rect.left <= -10000 {
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        let _ = unsafe { SetWindowPos(hwnd, None, 120, 120, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER) };
    }
    true.into()
}

fn hide_pid_windows(pid: u32) {
    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        let pid = param.0 as u32;
        let mut owner = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut owner));
        if owner == pid && IsWindowVisible(hwnd).as_bool() {
            hide_window(hwnd);
        }
        BOOL(1)
    }
    unsafe { let _ = EnumWindows(Some(visit), LPARAM(pid as isize)); }
}

#[cfg(windows)]
fn kill_all_live_captions() {
    let mut pids: Vec<u32> = Vec::new();
    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        let pids = &mut *(param.0 as *mut Vec<u32>);
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 && process_is_live_captions(pid) && !pids.contains(&pid) {
            pids.push(pid);
        }
        BOOL(1)
    }
    unsafe { let _ = EnumWindows(Some(visit), LPARAM(&mut pids as *mut Vec<u32> as isize)); }
    // 没有窗口的残留进程也一并结束（启动失败时窗口可能还没建出来）。
    if let Some(pid) = find_live_captions_pid() {
        if !pids.contains(&pid) { pids.push(pid); }
    }
    for pid in pids {
        unsafe {
            if let Ok(process) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                let _ = TerminateProcess(process, 1);
                let _ = CloseHandle(process);
            }
        }
    }
}

#[cfg(windows)]
unsafe fn hide_window(hwnd: HWND) {
    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
    let toolwindow = WS_EX_TOOLWINDOW.0 as i32;
    let layered = WS_EX_LAYERED.0 as i32;
    if ex_style & (toolwindow | layered) != (toolwindow | layered) {
        SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style | toolwindow | layered);
    }
    let _ = SetWindowDisplayAffinity(hwnd, windows::Win32::UI::WindowsAndMessaging::WINDOW_DISPLAY_AFFINITY(0x11) /* WDA_EXCLUDEFROMCAPTURE */);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 0, LWA_ALPHA);
    // WinUI 3 的 DirectComposition 会忽略分层 alpha，必须同时挪到屏幕外；
    // 实测离屏后 UIA 仍持续更新（history 有新增），不会中断识别。
    let _ = SetWindowPos(hwnd, None, -12000, -12000, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER);
}

/// 取消隐藏：恢复不透明、去掉工具窗口样式并还原窗口。
#[cfg(windows)]
unsafe fn restore_window(hwnd: HWND) {
    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
    let toolwindow = WS_EX_TOOLWINDOW.0 as i32;
    // 保留 WS_EX_LAYERED，只把 alpha 调回 255；先去掉分层再设 alpha 是无效的。
    if ex_style & toolwindow != 0 {
        SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style & !toolwindow);
    }
    let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA);
    let _ = SetWindowPos(hwnd, None, 80, 80, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mixed_chinese_english() {
        let sentences = split_sentences("你好。Hello world! 今天天气不错？");
        assert_eq!(sentences.len(), 3);
        assert_eq!(sentences[0].text, "你好。");
        assert!(sentences[0].final_);
        assert_eq!(sentences[1].text, "Hello world!");
        assert!(sentences[1].final_);
        assert_eq!(sentences[2].text, "今天天气不错？");
        assert!(sentences[2].final_);
    }

    #[test]
    fn short_sentences_never_produce_empty_items() {
        let sentences = split_sentences("Hi!  Hi?   Hi.");
        assert_eq!(sentences.len(), 3, "短句之间不应切出空句：{sentences:?}");
        assert!(sentences.iter().all(|s| !s.text.trim().is_empty()));
        assert!(sentences.iter().all(|s| s.final_));

        assert!(split_sentences("").is_empty());
        assert!(split_sentences("   \t\n").is_empty());
    }

    #[test]
    fn consecutive_duplicates_are_collapsed() {
        let sentences = split_sentences("好的。好的。Hello! Hello! 再见");
        let texts: Vec<&str> = sentences.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["好的。", "Hello!", "再见"]);
        assert!(sentences[0].final_);
        assert!(sentences[1].final_);
        assert!(!sentences[2].final_, "末尾无标点的残留必须是 final=false");
    }
}
