# 岛仔「实时字幕」页 — 嵌入 WhisperLiveKit 的实施计划

- 格式：agent-execution-plan/v2
- 文档状态：ready
- 目标：岛上新增第 12 页「字幕」。开启后采集系统声音（或麦克风），在本机跑 WhisperLiveKit 做流式识别，岛上实时显示**原文语言 + 原文**，可一键开启**翻译成目标语言**并同时显示译文；收起态可在岛角显示一行滚动字幕。
- 范围：只做本机 Windows 版的实时字幕（不做出声、不做会议纪要落库、不做说话人分离）。翻译走本机 NLLB-200，不联网、不上传音频。
- 工作目录：`D:\Daozai_fork`（分支 `feat/live-captions`，基线 `feat/download-center @ f73038f`）
- 目标环境：本机 Windows（`x86_64-pc-windows-msvc`），交付物 = `target/x86_64-pc-windows-msvc/release/daozai.exe`
- 操作边界：允许改 `src/`、`Cargo.toml`、`tools/`、`docs/`；不改 `D:\Daozai_verify`（另一 session 的 worktree）；不在用户机上留探针；侧车运行时装在 `%LOCALAPPDATA%\daozai\wlk`，可用设置项覆盖。

## 事实基线（实测，2026-11-22）

| 项 | 实测值 | 来源 |
|---|---|---|
| WhisperLiveKit | 0.2.28，Apache-2.0，`pip install whisperlivekit[translation]`（torch/faster-whisper/nllw） | `D:\_research\whisperlivekit`、`D:\_research\wlk-venv` |
| Python 要求 | `>=3.11,<3.14`；本机默认 3.14.2 不可用，取 `uv` 的 CPython 3.12.13 | `uv python list` |
| PCM 契约 | 16 kHz / 单声道 / s16le 裸 PCM，100 ms 一帧（3200 B） | `audio_processor.py`、POC |
| ASR 速度 | CPU int8/8 线程 beam=1 RTF **0.21**；GPU fp16 beam=1 RTF **0.035**；beam=5 分别 0.38 / 0.23 | `bench.py` / `bench_gpu.py` |
| GPU 可用条件 | `pip nvidia-cublas-cu12 + nvidia-cudnn-cu12`，把 `nvidia/*/bin` 加进 DLL 搜索路径后 CT2 4.9.0 正常 | `bench_gpu.py` |
| 崩溃根因 | WLK 默认 `device="auto"` 在无 cuBLAS 机器上会选中 CUDA → warmup 失败 → 服务拒绝启动 | `local_agreement/backends.py:117` |
| 翻译限制 | `--lan auto` + 非 simulstreaming 时 WLK 直接 `raise ValueError`；nllw 也不接受 `src_lang=auto` | `core.py:303-354`、`nllw` |
| 翻译模型 | `facebook/nllb-200-distilled-600M`（2.4 GB，首次下载后缓存），`nllw.load_model([src])` + `OnlineTranslation(model,[src],[tgt])` | `D:\_research\wlk-test\bench_mt.py` |
| 快照协议 | `FrontData.to_dict()`：`status` / `lines[{speaker,text,start,end,translation?,detected_language?}]` / `buffer_transcription` / `remaining_time_transcription`；结束 `{"type":"ready_to_stop"}` | `timed_objects.py` |
| 本机 CPU/GPU | 16 逻辑核；RTX 4060 Ti 8 GB（驱动 610.88） | 实测 |

**关键结论**：不能直接用 WLK 自带的 `--target-language`（它与 `--lan auto` 互斥）。做法是 **WLK 只负责 ASR（lan=auto），翻译在我们自己的桥接层按每句 `detected_language` 指定源语言**，并且翻译必须跑在独立 worker 队列里，绝不能阻塞 ASR 快照。

## 设计要点（契约，实现方按此落地）

1. **三层结构**：`Rust UI ← WebSocket(JSON) → Python 侧车 ← WLK AudioProcessor`。侧车把 WLK 的 `FrontData` 归一化成稳定 schema，Rust 端不依赖 WLK 内部字段漂移。
2. **侧车入口**：`tools/wlk_bridge/server.py`，`python -m wlk_bridge.server --port 0 --model base --device auto`。启动后把实际端口写进 stdout 一行 `WLK_READY {"port":8125,"asr_ready":true,"device":"cuda","model":"base"}`，Rust 据此连；`GET /health` 同义可供探活。
3. **WS 契约**（`/asr?language=auto&target_language=zh&translate=1`）：
   - 连接后第一帧 `{"type":"config","sample_rate":16000,"channels":1,"format":"s16le","mode":"full","translate":true}`；
   - 之后 `{"type":"captions","seq":7,"status":"active","lang":"en","lang_prob":0.99,"lines":[{"id":"s3","text":"...","translation":"...","lang":"en","start":1.2,"end":3.4,"final":true}],"partial":"...","buffer_translation":"..."}`，建议 150–250 ms 一帧（有变化才发，最多 5 帧/秒）；
   - 翻译晚到：同一 `id` 的行再用 `{"type":"translation","id":"s3","translation":"...","lang":"en"}` 补发，Rust 按 id 合并；
   - 致命错误 `{"type":"error","error":"..."}`；收尾 `{"type":"ready_to_stop"}`。
   - `lang` 取最近一句的 `detected_language`，用于岛上「原文语言」标签。
4. **翻译队列**：侧车内部 `queue.Queue` + 单 worker 线程；键 = `(id, src, tgt, text)`；同一 `src→tgt` 复用 `OnlineTranslation`；`translate=0` 时完全不加载 NLLB。首次加载在后台线程完成，加载期间照常出原文。
5. **设备选择**：`--device auto` → 先试 `cuda/fp16`（把 `site-packages/nvidia/*/bin` 加进 DLL 路径），失败回落 `cpu/int8`；`--device cpu` 强制 CPU。`beam_size` 默认 1（可用 `--beam` 覆盖），并 monkeypatch WLK 的 `FasterWhisperASR.transcribe` 以关掉 `condition_on_previous_text`，否则长会话语义漂移且变慢。
6. **音频采集**（Rust）：WASAPI loopback（默认设备）或麦克风，`IAudioClient` 共享模式，引擎格式多为 48 kHz float32 立体声 → 下混单声道 + 线性重采样到 16 kHz + f32→i16，100 ms 一帧推给侧车。采集线程只做搬运，不做阻塞 IO。
7. **Rust 模块**：
   - `src/caption_audio.rs`：`CaptureSource {Loopback, Microphone}`、`start(source) -> Result<Capture, String>`、`Capture { rx: Receiver<Vec<i16>>, … }`，Drop 时停线程。
   - `src/caption.rs`：`Captions` 句柄（`start/stop/snapshot/set_translation/restart`），后台线程持 `Arc<Mutex<CaptionState>>`；WS 用 `tungstenite`（已在依赖），读超时 20 ms 轮询：先排空音频 → 再读一帧 → 更新状态。断线指数退避重连（1/2/4/8 s，最多 3 次）。
   - `src/caption_runtime.rs`：侧车进程管理。解析 python（设置项 → `%LOCALAPPDATA%\daozai\wlk\venv\Scripts\python.exe` → `uv` 现场建 venv 并装依赖），spawn、读 `WLK_READY`、`/health` 探活、Drop 杀进程树。
   - `src/caption_ui.rs`：页面与设置面板。
8. **页面接入**：`CAPTION_PAGE = 11`（第 12 页），`CAPTION_H = 232`；按 `docs/research/2026-11-21-island-integration-map.md` 的清单改齐 `PAGE_NAMES`、`PAGE_SETTINGS_TABS`、`dots`、`render_page`、`page_title`、`expanded_height`、`arrow_y`、`model.rs` 两个数组 + `page_order` 迁移（`9→10→11→12`）、`extras.rs` 的 `SETTINGS_GROUPS`/`descriptions`/`settings_page_icon`/`settings_tab_module`/分页 match、`lib.rs` 模块声明、`ui_tests.rs` 里写死页数的断言。
9. **设置项**（`Settings` 四处齐全：字段 / Default / load / save）：
   `caption_enabled:bool`、`caption_autostart:bool`、`caption_source:u8`(0 系统声音/1 麦克风)、`caption_translate:bool`、`caption_target_lang:String`（默认 `zh`）、`caption_show_partial:bool`、`caption_on_island:bool`（收起态角标显示一行）、`caption_font_size:u8`(12–28)、`caption_python:String`（空=自动）、`caption_model:String`（默认 `base`）、`caption_device:u8`(0 auto/1 cpu)、`caption_beam:u8`(默认 1)。
10. **岛上呈现**：展开页里每条字幕两行——第一行小字「`EN` · 时间」，第二行原文；开启翻译后第三行译文（浅色）。顶部一行状态：`● 识别中 · 原文 EN · 译文 中文`；未启动时显示「未开启」+「启动」按钮。收起态（`caption_on_island`）用角标区显示最近一句原文（限宽 168 px，超出省略），译文可在设置里关掉。
11. **失败可见**：任何一层失败（python 缺失、依赖没装、模型下载中、WS 断开）都要在页面上给出**一句可执行的提示**（例如「点这里安装运行环境（约 2.5 GB）」），不允许静默空白。
12. **不做**：说话人分离、字幕落库、导出 SRT、云端 ASR、音频上传。

## 工作安排

### T01 — 分支与基线（已完成）
- 状态：done
- 改动：`git checkout -b feat/live-captions`（自 `feat/download-center @ f73038f`）。
- 验收：`git branch --show-current` → `feat/live-captions`。

### T02 — 侧车：归一化字幕 + 独立翻译 worker
- 状态：ready
- 依赖：无（只写 `tools/wlk_bridge/**`）
- 改动：`server.py`（asyncio + websockets 17，`websockets.serve` 必须在运行中的 loop 里创建）、`wlk_loader.py`（monkeypatch `FasterWhisperASR.load_model` 与 `transcribe`；CUDA DLL 注入 + CPU 回落）、`normalize.py`（`FrontData` → 上文第 3 条 schema）、`translate_worker.py`（队列 + 线程 + 按 `(src,tgt)` 缓存 `OnlineTranslation`）、`requirements.txt`、`README.md`。
- 产物：能在 `D:\_research\wlk-venv` 直接跑起来的侧车；`python tools\wlk_bridge\selftest.py --wav D:\_research\wlk-test\en1_16k.wav --target zh` 打印出原文 + 译文 + 语言。
- 验收：selftest 退出码 0，输出包含 `Hello everyone` 与非空中文译文；`--device cpu` 与 `--device auto` 都能起。

### T03 — Rust：WASAPI 环回采集 + 重采样
- 状态：ready
- 依赖：无（只写 `src/caption_audio.rs`）
- 改动：`CaptureSource`、`Capture`、`start()`；48k/44.1k float32 立体声 → 16k mono i16；`#[cfg(test)]` 覆盖重采样长度/直流/幅度与下混。
- 验收：`cargo test --lib caption_audio --features record -- --test-threads=1` 全绿；`--ignored live_` 的真机测试能采 2 秒非静音。

### T04 — Rust：字幕客户端 + 状态机 + 侧车进程
- 状态：ready
- 依赖：T02（协议）、T03（音频源）
- 改动：`src/caption.rs`、`src/caption_runtime.rs`；`Cargo.toml` 若需 `serde` 派生已在；WS URL、重连、状态合并、错误透传。
- 验收：`cargo test --lib caption:: --features record -- --test-threads=1`；手动跑 `cargo run` 后页面能进入「识别中」。

### T05 — Rust：第 12 页 UI + 设置项 + 迁移
- 状态：ready
- 依赖：T04（数据源）
- 改动：`src/caption_ui.rs` + 第 8/9 条清单里的所有接入点。
- 验收：`cargo test --locked --all-targets --features record -- --test-threads=1` 全绿；岛上能看到「字幕」页与设置分页。

### T06 — 实机验证
- 状态：ready
- 依赖：T05
- 内容：①用 TTS 生成的中英混合音频经系统播放（环回采集）验证原文语言切换与译文；②真实视频/会议软件播放 60 s，记录首字延迟与断线恢复；③翻译开关热切换；④杀进程后重启恢复。
- 验收：截图/录屏 + `caption_selftest` 日志；岛上同时出现原文与译文，语言标签正确。

### T07 — 交付
- 状态：ready
- 内容：`cargo build --release` → 覆盖 `target/x86_64-pc-windows-msvc/release/daozai.exe`，核对桌面快捷方式目标与哈希，若在运行则重启并核对进程路径；`git push -u origin feat/live-captions`。
- 验收：`git ls-remote origin feat/live-captions`；快捷方式目标哈希 == 新 exe 哈希。

## 风险与对策

| 风险 | 对策 |
|---|---|
| 首次安装 2.5 GB 依赖/模型，用户以为卡死 | 页面上给进度与预计大小；安装走后台线程 + 可取消 |
| 无 NVIDIA 显卡 | 自动回落 CPU int8（RTF 0.21，仍可实时）；设置项可强制 CPU |
| WLK 内部 API 漂移 | 侧车只依赖 `WhisperLiveKitConfig` / `TranscriptionEngine` / `AudioProcessor` 三个入口，版本锁 `0.2.28` |
| 长会话 lag 累积 | `vac=True` + `buffer_trimming=sentence` + 15 s 上限；实测 lag 超 3 s 时侧车主动提示 |
| 翻译抖动 | 只翻 `final=true` 的句子；同一句重复翻译按 `(id,text)` 去重 |
| 与另一 session 冲突 | 只在 `D:\Daozai_fork` 的 `feat/live-captions` 上改；不碰 `D:\Daozai_verify` |
