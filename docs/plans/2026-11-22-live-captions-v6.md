# 实时字幕 v6：滚动文本窗、遮挡修复与构建缓存治理

日期：2026-11-22
分支：`feat/live-captions`
状态：实施中（本文件随代码一起提交）

## 1. 用户反馈与根因

| # | 现象 | 根因（Observed） |
|---|------|------------------|
| 1 | 日期/时钟压在字幕文字上 | 字幕分支画完没有 `return`，继续走顶栏电池/日期/时钟绘制 |
| 2 | 没开翻译却看到四行、文字互相重叠 | 按「一句一行」排版，长句各自换行成两行；56/96 高度只按单行算，第二行压到下一句 |
| 3 | 宽度拉到最大 UI 错乱、左侧圆角消失 | 滑块上限 720 + 吉祥物 48 + 番茄钟 70 > 画布 `WIN_W=640`，药丸溢出窗口 |
| 4 | `[ZH]` 被挪到吉祥物右边，不好看 | 上一版为了避让直接 `left+96`，位移过大 |
| 5 | 点设置切换时会闪出微软实时字幕窗口 | 停止字幕时 `restore_window()` 把 LiveCaptions 恢复显示，重启再隐藏，形成一次闪现 |
| 6 | 构建缓存占用几十 GB | `target/` 累积了 debug + msvc 两套 ~25 GB 中间产物 |

## 2. 设计契约

- **C1 滚动文本窗**：岛上不是「一句一行」，而是一个连续文本窗口——把本场句子连成一段，按宽度换行，只显示最后 2 行；开启翻译时原文 2 行 + 译文 2 行（共 4 行）。这样长句换行不会变成四行原文。
- **C2 高度固定**：纯原文 56 px（2 行 × 17 + 边距），四行 96 px；字号固定 12/11，不缩字、不加省略号。
- **C3 动画**：只在「换句」（最后一句 id 变化）时做 0.22 s、8 px 的上滑；同一句增长时不动画，避免抖动。
- **C4 顶栏让位**：字幕条显示时直接 `return`，不画电池/日期/时钟；番茄钟 focus bar 仍画在右侧。
- **C5 宽度**：设置范围 360–616 px（画布 640 − 24 边距）；`caption_bar_width()` 再夹紧到 `WIN_W-24`，确保圆角完整。超过画布的要求后续用独立全宽窗口实现，不在本版强行放开。
- **C6 标签位置**：`[ZH]` 固定在文本起点左侧；有吉祥物时从 `left+64` 起排（不再 96），只往左让一点。
- **C7 无感隐藏**：`stop()` 不再恢复 LiveCaptions 窗口；轮询每次发现窗口可见就压回 `alpha=0`，用户用 `Win+Ctrl+L` 重新打开也会被立刻隐藏。
- **C8 构建缓存**：交付后删除 `target/debug`、`target/release` 与 `target/x86_64-pc-windows-msvc` 下的 `debug/deps/build/incremental`，只保留 `release/daozai.exe` 作为桌面快捷方式目标。

## 3. 工作分解

- T01 `src/app.rs`：字幕段末尾 `return` + focus bar；连续文本窗口；固定字号与无省略号换行；`caption_key` 只认最后一句 id。
- T02 `src/app.rs`：`caption_bar_width()` 夹紧到 `WIN_W-24`；`src/caption_ui.rs` 滑块上限 616。
- T03 `src/live_captions.rs`：`stop()` 不恢复窗口；`try_find_window()` 每次轮询都隐藏；`restore_window()` 保留 layered、alpha=255、`SW_SHOWNOACTIVATE`。
- T04 `src/caption.rs`：live 行 id 加会话前缀（避免复用 `live-part-1` 覆盖旧记录）；更新行时先摘除再追加（保持快照顺序）；`existed_done` 只看 `index < session_start`；final 到达时清掉同文本的半句行。
- T05 单测：`cargo test --lib caption`、`live_captions`、`tools/wlk_bridge/test_normalize.py`。
- T06 release 构建 → 启动核对 → 构建缓存清理 → 提交推送。

## 4. 验收

1. 字幕页与岛上：无日期/时钟遮挡；纯原文最多 2 行、翻译时最多 4 行且互不重叠。
2. 宽度 616 px 时药丸圆角完整、不越界。
3. `[ZH]` 紧贴文本左端，不再被吉祥物压住。
4. 切换翻译/引擎时 LiveCaptions 窗口不闪现；任务栏/Alt-Tab 看不到它。
5. `target/` 交付后 < 1 GB（保留可执行文件）。
6. 服务端/客户端单测全绿，release 启动后 UIA 取字与翻译链路正常。
