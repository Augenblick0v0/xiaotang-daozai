# wlk_bridge — WhisperLiveKit 侧车

给「岛仔」桌面端提供实时字幕的 Python 侧车：16 kHz 单声道 PCM 进，字幕快照 + 中文译文出。
不依赖 WLK 自带翻译（`language=auto` 与 `target_language` 互斥），翻译由独立线程跑 NLLB-200。

## 安装

```powershell
# 已有 venv 可跳过第一行
uv venv D:\_research\wlk-venv
uv pip install --python D:\_research\wlk-venv\Scripts\python.exe -r tools\wlk_bridge\requirements.txt
# 有 NVIDIA GPU 时再装（否则 --device auto 会回落 cpu/int8）：
uv pip install --python D:\_research\wlk-venv\Scripts\python.exe nvidia-cublas-cu12 nvidia-cudnn-cu12
```

模型缓存默认 `D:\_research\wlk-cache`（faster-whisper base + NLLB-600M）。

## 运行

```powershell
D:\_research\wlk-venv\Scripts\python.exe tools\wlk_bridge\server.py `
  --host 127.0.0.1 --port 8125 --model base --device auto --beam 1 `
  --cache-dir D:\_research\wlk-cache --nllb-size 600M
```

模型 warmup 完成后 stdout 打一行（供 Rust 解析）：

```
WLK_READY {"port":8125,"asr_ready":true,"device":"cuda","model":"base"}
```

探活：`curl http://127.0.0.1:8125/health`

## 协议

WebSocket：`ws://127.0.0.1:8125/asr?language=auto&target_language=zh&translate=1`

| query | 默认 | 说明 |
|---|---|---|
| `language` | `auto` | 识别语言；`auto` 由模型检测 |
| `target_language` | 空 | 目标语言，如 `zh`；为空不翻译 |
| `translate` | `0` | `1` 开启逐句翻译 |
| `context` | 空 | 术语/提示词（≤1000 字符） |

客户端只发二进制帧：16 kHz、单声道、s16le PCM。发空二进制帧表示结束。

服务端第一帧：

```json
{"type":"config","sample_rate":16000,"channels":1,"format":"s16le","mode":"full","translate":true}
```

之后有变化才发字幕快照（最多 5 帧/秒）：

```json
{"type":"captions","seq":7,"status":"active","lang":"en","lang_prob":0.99,
 "lines":[{"id":"L0000-00000000","text":"Hello everyone...","translation":"","lang":"en","start":0.0,"end":2.4,"final":true}],
 "partial":"...","buffer_translation":""}
```

翻译晚到，按 `id` 合并：

```json
{"type":"translation","id":"L0000-1a2b3c","translation":"大家好……","lang":"en"}
```

`lang` 取最近一句的 `detected_language`（没有就沿用上一次）。`start`/`end` 是秒（float）。
`lang_prob` 来自 faster-whisper 的 `language_probability`（补丁把 `info` 保留在 ASR 实例上）。

结束时服务端发 `{"type":"ready_to_stop"}`；致命错误发 `{"type":"error","error":"..."}`。

## 自测

```powershell
D:\_research\wlk-venv\Scripts\python.exe tools\wlk_bridge\selftest.py `
  --wav D:\_research\wlk-test\en1_16k.wav --target zh
```

会自己起 `server.py`、等 `WLK_READY`、按 100 ms 推 PCM，最后打印原文/语言/译文。断言至少一条非空
`lines[].text` 和一条非空译文；退出码 0/1。`--device cpu` 可强制走 CPU。

## 备注

- CUDA 不可用时自动回落 `cpu/int8`（`--device cpu` 强制 CPU）。
- 翻译模型在 worker 线程首次收到句子时才加载，不阻塞字幕；`translate=0` 时永不加载。
- NLLB 默认只读本地缓存（`WLK_NLLB_ALLOW_DOWNLOAD=1` 才允许联网下载）。
- 仅面向本机回环使用，没有鉴权。
