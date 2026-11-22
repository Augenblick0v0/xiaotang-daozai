# 实时字幕 v4：接入 Windows 实时字幕（Live Captions）

日期：2026-11-22
状态：已确认，执行中

## 背景

v3 的 Whisper 流式链路能出字，但分句/标点质量与系统自带「实时辅助字幕」差距明显：本机实测
`LiveCaptions.exe` 的 `CaptionsTextBlock` 通过 UI Automation 暴露**整段带标点的滚动文本**，
开源项目 SakiRinn/LiveCaptions-Translator 已用同样方式稳定取词（读 `AutomationId=CaptionsTextBlock`
的 `Current.Name`，10–20 ms 一次），并把窗口 `SW_MINIMIZE + WS_EX_TOOLWINDOW` 隐藏后继续识别。

## 目标

1. 新增「识别来源 = Windows 实时字幕」可选引擎：后台启动并隐藏 Live Captions，轮询文本，
   按句末标点切句，岛上/页面显示两行**已定稿**原文（上一句、最新一句），翻译按句回填。
2. 保留现有 Whisper 引擎作为回退；Live Captions 不可用时自动回退并提示。
3. 翻译仍由本机 NLLB 完成（侧车新增 translate-only 模式，不加载 Whisper）。

## 设计契约

- C1 `src/live_captions.rs`：`LiveCaptions::start() -> Result<LiveSession,String>`；后台线程
  - `Command::new("LiveCaptions.exe").spawn()`，按 pid 找 `ClassName=LiveCaptionsDesktopWindow`；
  - `CoCreateInstance(CUIAutomation)` → root → `FindFirst(Children, ProcessId)` →
    `FindFirst(Descendants, AutomationId="CaptionsTextBlock")`；
  - 每 120 ms 读 `CurrentName()`；元素失效重新查找；进程退出则 2 s 后重启；
  - 隐藏：`ShowWindow(SW_MINIMIZE)` + `SetWindowLong(GWL_EXSTYLE, WS_EX_TOOLWINDOW)`；
  - `stop()` 恢复窗口并结束本进程启动的 LiveCaptions。
- C2 分句：`split_sentences(text) -> Vec<String>` 按 `.?!。？！…` 切；短句（<10 字节）并回上一句；
  只把「以句末标点结尾」的句子标 final；连续相同句去重；保留最近 200 行。
- C3 会话：`LiveUpdate{lines:Vec<LiveLine>, partial:String, lang_hint:String}` 通过
  `std::sync::mpsc` 推给 App，`CaptionTick` 每帧 try_recv。
- C4 翻译：侧车 `server.py --translate-only` 只加载 NLLB，暴露 `POST /translate`
  `{"text","src","tgt"}` → `{"translation"}`；Rust 用 `ureq` 调用（已依赖）。
- C5 设置：新增 `caption_engine: u8`（0=Whisper，1=Windows 实时字幕），默认 0；页面加「引擎」开关；
  切引擎需重启会话。
- C6 岛：两行均为结果（上一句 / 最新一句），有译文时下行显示译文；
  不再把 partial 画成第二行；宽度按句截断。
- C7 回退：Live Captions 启动/找控件失败 → 页面显示「未检测到系统实时字幕」并提供「改用 Whisper」。

## 外观（用户补充要求，一并做）

- 岛上两行**都是结果**：第一行上一句、第二行最新一句；有译文时第二行替换为译文（原文仍保留可回看）。
- 句子级截断：超宽时按句末/逗号截断并加省略号，不从词中间切；不再「显示不下才换行」。
- 中文按全角标点、英文按半角标点排版；行高 20，字号 12/10.5 自适应，标签 `[ZH]` 固定宽度不抖动。
- 页面记录区：每条一行原文 + 一行译文，句间 6px；页脚保留语种/句数/引擎。

## 任务分解

- T01 计划与契约（本文件）
- T02 `tools/wlk_bridge/server.py --translate-only` + `/translate` + 自测
- T03 `src/live_captions.rs`：UIA 启动/隐藏/轮询/分句 + 单测（分句、短句合并、去重）
- T04 接线 `caption.rs`（engine 分支）、`caption_ui.rs`（引擎选择、状态文案）、`app.rs`
- T05 构建 + 单测 + 静音实机验证（WAV 注入不适用于 Live Captions，改用系统播放时人工确认）
- T06 提交推送 + 更新本地运行版

## 风险

- `AutomationId` 随 Windows 更新变化 → 失败回退 Whisper。
- Live Captions 语言由系统设置决定，不随岛仔设置切换 → 页面提示「语言在系统实时字幕设置里选」。
- 隐藏窗口后个别版本可能停更 → 保留「显示系统字幕窗口」开关用于排障。
