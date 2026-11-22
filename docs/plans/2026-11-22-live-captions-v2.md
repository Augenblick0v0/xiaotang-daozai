# 实时字幕 v2：中文可用性、语言选择、两行灵动岛与识别记录

- 日期：2026-11-22
- 分支：`feat/live-captions`（继续，不新开）
- 输入：用户 m01708 的四条反馈 + 三张截图（字幕页长句被裁、药丸一行繁体长串、13 句只显示 3 句）
- 约束：**不做任何鼠标/键盘自动化测试**（用户正在用电脑）；验证只用单测、WAV 静音注入、`PrintWindow` 截图

## 1. 问题归因（Observed）

| # | 现象 | 根因 |
|---|---|---|
| 1 | 中文一直是一句话、字一个个往上冒 | `normalize.py` 只按 `.!?。！？` 切句；中文 ASR 常无标点 → 整段永远是「remainder」，随 WLK 快照逐字增长；Rust 侧 `captions` 帧直接 **整体替换** `view.lines`，所以只能看到最新快照里的 3 行 |
| 2 | 无法选识别语言，简体识别成繁体 | WS query 写死 `language=auto`；`faster-whisper` 对 `zh` 默认可能输出繁体，且没有 `context`（initial_prompt）提示 |
| 3 | 长句看不到后半段；岛上只有一行 | 药丸高度固定 `BAR_H=30`、单行绘制，超宽直接裁掉 |
| 4 | 页脚 13 句、只看到 3 句；无历史/导出 | 同上「整体替换」；没有持久化与导出 |

## 2. 设计契约

1. **语言选择**：`caption_lang ∈ {zh, en, mixed, ja, ko}`，默认 `zh`；映射到 WS `language=zh|en|auto|ja|ko`。`zh` 时附带 `context=以下是普通话的句子，请使用简体中文。`（同时改善简繁与标点）。
2. **翻译跳过同语**：源语言 == 目标语言时不下发翻译请求，UI 标注「原文已是目标语言」，避免中文→中文的胡译。
3. **中文分段**：`normalize.py` 三级切分——句末标点（。！？!?…；;）→ 逗号级（，,、:：）且片段 > 18 字 → 硬切 24 字（CJK）/48 字符（拉丁）。id 固定为 `{base}-{pos:02d}`（不再带文本哈希，保证长句增长时同一位置 id 稳定）。
4. **历史合并**：Rust `caption.rs` 维护 `history: Vec<Line>`（按 id 合并、更新文本/译文、上限 2000 行），`view.lines` = 最近 3 行给面板，`view.history` = 全量给「记录」视图。
5. **两行灵动岛**：字幕激活时药丸高度 30→54；第一行原文、第二行译文（未开翻译时第二行放原文的后续部分）；每行超宽用尾部省略。
6. **持久化**：`%APPDATA%\Daozai\caption-history.jsonl`，追加写；`caption_history_mb`（默认 20）超过即从最旧裁剪；启动时加载最近记录。
7. **导出**：TXT（`[HH:MM:SS] 原文 / 译文`）与 SRT（按 start/end）；默认写到桌面，文件名 `字幕记录-YYYYMMDD-HHMMSS.txt/.srt`，导出后在页面显示路径。
8. **清空**：页面提供「清空记录」，二次确认（同一个按钮点两次生效，5 秒窗口）。

## 3. 工作分解

| ID | 内容 | 文件 | 负责 |
|---|---|---|---|
| T01 | 中文友好分段 + 稳定 id + zh 简体提示 + 同语不翻译 | `tools/wlk_bridge/normalize.py`、`server.py`、`test_normalize.py` | 子 agent `py-captions` |
| T02 | 历史存储模块（环形缓冲、JSONL、TXT/SRT 导出、上限） | `src/caption_history.rs` | 子 agent `history-store` |
| T03 | 客户端历史合并 + 语言参数 + 同语跳过 | `src/caption.rs`、`src/caption_runtime.rs` | lead |
| T04 | 设置项与页面：语言选择、记录列表/滚动、导出、清空、上限 | `src/model.rs`、`src/caption_ui.rs`、`src/extras.rs` | lead |
| T05 | 灵动岛两行布局（高度/换行/省略） | `src/app.rs` | lead |
| T06 | 单测 + 侧车 WAV 回归 + PrintWindow 实拍 | `src/ui_tests.rs`、`tools/wlk_bridge/selftest.py` | lead + 子 agent `qa-verify` |
| T07 | 提交推送 + 覆盖本地运行版 | git / `target\x86_64-pc-windows-msvc\release\daozai.exe` | lead |

## 4. 验收标准

- 中文连续说话 30 s：出现 ≥6 条独立短句，每条 ≤ 24 个汉字，不再是一条无限增长的长句。
- 选「中文」时输出简体；选「English」时英文识别；「中英混合」按 auto 逐段判定。
- 岛上药丸高度 54，两行；开启翻译时上原文下译文。
- 字幕页可滚动查看本次全部记录；页脚句数与列表一致；导出 TXT/SRT 文件存在且内容含时间与文本。
- `cargo test --lib` 全绿；侧车 `selftest.py` PASS。

## 5. 风险

- WLK 的 `context` 会进入 decoder prompt，过长会拖慢；限制 40 字以内。
- 两行药丸会改变窗口高度，需同步 `island_ticker_width`/命中区域，避免点击错位。
- 历史写盘在主线程之外（后台线程批量 flush），避免 UI 卡顿。
