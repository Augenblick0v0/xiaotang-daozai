//! 字幕侧车的进程管理：把 `tools/wlk_bridge` 里的 Python 源码嵌进 exe，
//! 需要时写出来、建 venv、装依赖、起服务，并从 stdout 的 `WLK_READY` 里拿端口。
//!
//! 为什么嵌进 exe：岛仔是单文件绿色版，不能让用户再去找一份 Python 目录。
//! 运行时把脚本写到 `%LOCALAPPDATA%\daozai\wlk\bridge`，venv 也放在同一层，
//! 卸载时删这一个目录就干净了。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 侧车源码（与仓库 `tools/wlk_bridge` 同源，改一处即可）。
const SERVER_PY: &str = include_str!("../tools/wlk_bridge/server.py");
const LOADER_PY: &str = include_str!("../tools/wlk_bridge/wlk_loader.py");
const NORMALIZE_PY: &str = include_str!("../tools/wlk_bridge/normalize.py");
const TRANSLATE_PY: &str = include_str!("../tools/wlk_bridge/translate_worker.py");
const REQUIREMENTS: &str = include_str!("../tools/wlk_bridge/requirements.txt");

/// 侧车源码的版本指纹：源码一改就跟着变，用来决定要不要重写磁盘上的副本。
fn source_stamp() -> u64 {
    let mut hash = 1469598103934665603u64;
    for text in [SERVER_PY, LOADER_PY, NORMALIZE_PY, TRANSLATE_PY, REQUIREMENTS] {
        for byte in text.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(1099511628211);
        }
    }
    hash
}

/// 运行目录：`%LOCALAPPDATA%\daozai\wlk`。
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("daozai").join("wlk")
}

fn bridge_dir() -> PathBuf {
    data_dir().join("bridge").join("wlk_bridge")
}

/// venv 里的解释器。
pub fn venv_python() -> PathBuf {
    data_dir().join("venv").join("Scripts").join("python.exe")
}

/// 模型缓存：允许用环境变量指到已有缓存（开发机就是这么复用的）。
fn model_cache() -> PathBuf {
    std::env::var_os("DAOZAI_WLK_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir().join("models"))
}

/// 运行环境是不是已经装好（venv + 侧车脚本）。
pub fn installed() -> bool {
    venv_python().is_file() && bridge_dir().join("server.py").is_file()
}

/// 把嵌入的源码写到磁盘。内容没变就不动，避免每次启动都写盘。
pub fn ensure_sources() -> Result<PathBuf, String> {
    let dir = bridge_dir();
    std::fs::create_dir_all(&dir).map_err(|error| format!("建目录失败：{error}"))?;
    let stamp_path = dir.join(".stamp");
    let stamp = source_stamp().to_string();
    if std::fs::read_to_string(&stamp_path).ok().as_deref() != Some(stamp.as_str()) {
        let files: [(&str, &str); 4] = [
            ("server.py", SERVER_PY),
            ("wlk_loader.py", LOADER_PY),
            ("normalize.py", NORMALIZE_PY),
            ("translate_worker.py", TRANSLATE_PY),
        ];
        for (name, body) in files {
            std::fs::write(dir.join(name), body).map_err(|error| format!("写 {name} 失败：{error}"))?;
        }
        std::fs::write(dir.join("requirements.txt"), REQUIREMENTS)
            .map_err(|error| format!("写 requirements.txt 失败：{error}"))?;
        std::fs::write(dir.join("__init__.py"), "")
            .map_err(|error| format!("写 __init__.py 失败：{error}"))?;
        let _ = std::fs::write(stamp_path, stamp);
    }
    Ok(dir)
}

/// 装运行环境（uv 建 venv + 装依赖）。`stage` 会被高频调用，用来把进度显示到岛上。
///
/// 这个函数是同步的、可能要跑几分钟，调用方必须在后台线程里调。
pub fn install(stage: &dyn Fn(&str), stop: &AtomicBool) -> Result<(), String> {
    ensure_sources()?;
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|error| format!("建目录失败：{error}"))?;
    let uv = find_uv().ok_or_else(|| {
        "没找到 uv。请先装 uv（PowerShell 里跑 `winget install astral-sh.uv`），再回来点安装。".to_string()
    })?;

    stage("正在创建 Python 3.12 运行环境…");
    let venv = dir.join("venv");
    let status = run_streamed(
        &uv,
        &[
            "venv".into(),
            venv.display().to_string(),
            "--python".into(),
            "3.12".into(),
        ],
        &dir,
        stage,
        stop,
    )?;
    if !status {
        return Err("创建 Python 环境失败（uv venv 返回非零）".into());
    }
    if stop.load(Ordering::SeqCst) {
        return Err("已取消".into());
    }

    stage("正在下载识别与翻译依赖（首次约 2.5 GB，请耐心等）…");
    let requirements = bridge_dir().join("requirements.txt");
    let status = run_streamed(
        &uv,
        &[
            "pip".into(),
            "install".into(),
            "--python".into(),
            venv_python().display().to_string(),
            "-r".into(),
            requirements.display().to_string(),
            "--index-strategy".into(),
            "unsafe-best-match".into(),
            "--extra-index-url".into(),
            "https://download.pytorch.org/whl/cpu".into(),
        ],
        &dir,
        stage,
        stop,
    )?;
    if !status {
        return Err("安装依赖失败（uv pip install 返回非零），详情见 sidecar.log".into());
    }
    stage("运行环境已就绪");
    Ok(())
}

/// 找 uv：PATH 里、或用户目录下的常见安装位置。
fn find_uv() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for part in std::env::split_paths(&path) {
            let candidate = part.join("uv.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from)?;
    for candidate in [home.join(".cargo").join("bin").join("uv.exe"), home.join(".local").join("bin").join("uv.exe")] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 跑一个命令并把输出按行喂给 `stage`；返回是否成功。被杀时返回 false。
fn run_streamed(
    program: &Path,
    args: &[String],
    cwd: &Path,
    stage: &dyn Fn(&str),
    stop: &AtomicBool,
) -> Result<bool, String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .spawn()
        .map_err(|error| format!("启动 {} 失败：{error}", program.display()))?;
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<String>();
    if let Some(stdout) = stdout {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);
    let mut tail = String::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = child.kill();
            return Ok(false);
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    tail.push_str(trimmed);
                    tail.push('\n');
                    if tail.len() > 4000 {
                        tail = tail.split_off(tail.len() - 2000);
                    }
                    // 依赖安装的输出很吵，只挑对用户有意义的行做状态。
                    if let Some(progress) = progress_line(trimmed) {
                        stage(&progress);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    return Ok(status.success());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = child.wait().map_err(|error| format!("等待进程失败：{error}"))?;
                if !status.success() {
                    append_log(&tail);
                }
                return Ok(status.success());
            }
        }
    }
}

/// 把 uv 的输出翻译成一句人话；返回 None 表示这行不值得显示。
fn progress_line(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("downloading ") {
        let name = rest.split_whitespace().next().unwrap_or("");
        return Some(format!("正在下载依赖包：{name}"));
    }
    if lower.contains("resolved") {
        return Some(format!("依赖解析完成：{}", line.trim_start_matches("Resolved ")));
    }
    if lower.starts_with("installed ") {
        return Some("依赖安装完成，正在收尾…".into());
    }
    if lower.contains("error") || lower.contains("failed") {
        return Some(format!("安装出错：{}", truncate(line, 90)));
    }
    None
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "…"
}

fn append_log(text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let path = data_dir().join("sidecar.log");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{text}");
    }
}

/// 起好的侧车。Drop 时杀掉进程（连带它的线程）。
pub struct Sidecar {
    pub port: u16,
    pub device: String,
    pub model: String,
    child: Child,
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 启动侧车并等它 ready。
///
/// `stage` 收进度文案；`stop` 用来中途取消。
pub fn start(
    python_override: &str,
    model: &str,
    force_cpu: bool,
    beam: u8,
    translate: bool,
    translate_only: bool,
    stage: &dyn Fn(&str),
    stop: &AtomicBool,
) -> Result<Sidecar, String> {
    let python = resolve_python(python_override)?;
    let dir = ensure_sources()?;
    let cache = model_cache();
    let _ = std::fs::create_dir_all(&cache);
    stage("正在启动识别引擎…");
    let mut args: Vec<String> = vec![
        "-u".into(),
        "-m".into(),
        "wlk_bridge.server".into(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        "0".into(),
        "--model".into(),
        model.to_string(),
        "--device".into(),
        if force_cpu { "cpu".into() } else { "auto".into() },
        "--beam".into(),
        beam.to_string(),
        "--cache-dir".into(),
        cache.display().to_string(),
    ];
    if translate_only {
        // Windows 实时字幕自己负责识别，侧车只当翻译服务用。
        args.push("--translate-only".into());
    }
    if translate {
        // 翻译模型加载要十几秒；提前在后台预热，第一句译文不用干等。
        args.push("--preload-translation".into());
    }
    let mut child = Command::new(&python)
        .args(&args)
        // `-m wlk_bridge.server` 需要把包所在的上一级目录放进 sys.path。
        .current_dir(dir.parent().unwrap_or(&dir))
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONUTF8", "1")
        .env("HF_HOME", &cache)
        .env("HF_HUB_DISABLE_SYMLINKS_WARNING", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|error| format!("启动 Python 失败：{error}（{}）", python.display()))?;

    // stderr 单独排空，否则管道塞满会把子进程卡死。
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut sink = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sink.len() < 8192 {
                    sink.push_str(&line);
                    sink.push('\n');
                }
                if line.contains("Error") || line.contains("error") {
                    append_log(&line);
                }
            }
        });
    }

    let stdout = child.stdout.take().ok_or("拿不到侧车输出")?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let text = line.trim_end().to_string();
                    if tx.send(text).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(600);
    let mut transcript = String::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = child.kill();
            return Err("已取消".into());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("识别引擎 10 分钟还没起来，已放弃（模型可能没下载完）".into());
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                if let Some(json) = line.strip_prefix("WLK_READY ") {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
                        let port = value.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                        if port == 0 {
                            let _ = child.kill();
                            return Err("侧车没报端口".into());
                        }
                        let device = value.get("device").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                        let model = value.get("model").and_then(|v| v.as_str()).unwrap_or(model).to_string();
                        return Ok(Sidecar { port, device, model, child });
                    }
                }
                if let Some(progress) = sidecar_progress(&line) {
                    stage(&progress);
                }
                transcript.push_str(&line);
                transcript.push('\n');
                if transcript.len() > 8000 {
                    transcript = transcript.split_off(transcript.len() - 4000);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    append_log(&transcript);
                    return Err(format!(
                        "侧车启动失败（退出码 {:?}），详情见 {}",
                        status.code(),
                        data_dir().join("sidecar.log").display()
                    ));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.wait();
                append_log(&transcript);
                return Err(format!(
                    "侧车退出了，详情见 {}",
                    data_dir().join("sidecar.log").display()
                ));
            }
        }
    }
}

/// 侧车启动时打在 stdout 上的人话（它自己会带 `[stage]` 前缀）。
fn sidecar_progress(line: &str) -> Option<String> {
    let rest = line.strip_prefix("[stage] ")?;
    Some(rest.trim().to_string())
}

/// 找解释器：设置里的覆盖值 → 已装好的 venv → 报错让用户去装。
pub fn resolve_python(python_override: &str) -> Result<PathBuf, String> {
    let trimmed = python_override.trim();
    if !trimmed.is_empty() {
        let path = PathBuf::from(trimmed);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("设置里指定的 Python 不存在：{}", path.display()));
    }
    if let Some(from_env) = std::env::var_os("DAOZAI_WLK_PYTHON") {
        let path = PathBuf::from(from_env);
        if path.is_file() {
            return Ok(path);
        }
    }
    let venv = venv_python();
    if venv.is_file() {
        return Ok(venv);
    }
    Err(format!(
        "还没装识别环境。点下面的「安装识别环境」按钮，或手动跑：uv venv \"{}\" --python 3.12",
        data_dir().join("venv").display()
    ))
}

/// 侧车日志路径（页面上「打开日志」用）。
pub fn log_path() -> PathBuf {
    data_dir().join("sidecar.log")
}

/// 读日志尾部（出错时直接展示几行，省得用户去找文件）。
pub fn log_tail(limit: usize) -> String {
    let Ok(mut file) = std::fs::File::open(log_path()) else {
        return String::new();
    };
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(limit);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_sources_are_present() {
        assert!(SERVER_PY.contains("WLK_READY"));
        assert!(!REQUIREMENTS.trim().is_empty());
    }

    #[test]
    fn stamp_changes_with_content() {
        // 只是个防回归：指纹非零，且两次调用稳定。
        assert_ne!(source_stamp(), 0);
        assert_eq!(source_stamp(), source_stamp());
    }

    #[test]
    fn progress_lines_are_translated() {
        assert!(progress_line("Downloading torch-2.15.0+cpu (200.1MiB)").unwrap().contains("正在下载"));
        assert!(progress_line("Resolved 42 packages in 1.2s").unwrap().contains("依赖解析完成"));
        assert!(progress_line("some random noise").is_none());
    }
}
