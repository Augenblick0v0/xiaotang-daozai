# Ghost Downloader 3 运行时接口探测报告

- 日期：2026-11-22 02:25–02:40（Asia/Hong_Kong）
- 目标：`Ghost-Downloader-3.exe`，PID **52156**，路径 `D:\Ghost Downloader\Ghost-Downloader-3.exe`，版本 **4.5.11.0**
- 目的：确认它是否对外暴露可读取**下载进度**与**下载速度**的本地接口，并用真实限速下载观察数值是否随时间变化
- 结论：**能**。唯一可用通道是 **14370 端口的 WebSocket**（浏览器扩展桥）：`hello` 认证 + `subscribe_tasks` 后，每秒收到 `task_snapshot`，其中 `progress` 为 **0–100 的百分比**、`speed` 为 **bytes/s**、`receivedBytes` 为已完成字节。16800 的 Aria2 门面没有查询方法；`tasks.jsonl` 下载中进度/速度恒 0，仅完成时跳 100。

---

## 1. 监听端口清单

### 1.1 原始命令输出

```text
PS> Get-NetTCPConnection -OwningProcess 52156 | Sort-Object LocalPort,LocalAddress | Select-Object LocalAddress,LocalPort,RemoteAddress,RemotePort,State,OwningProcess | Format-Table -AutoSize

LocalAddress LocalPort RemoteAddress RemotePort       State OwningProcess
------------ --------- ------------- ----------       ----- -------------
::1              14370 ::                     0      Listen         52156
127.0.0.1        14370 0.0.0.0                0      Listen         52156
::1              16800 ::                     0      Listen         52156
127.0.0.1        16800 0.0.0.0                0      Listen         52156
127.0.0.1        46005 127.0.0.1          46006 Established         52156
0.0.0.0          46006 0.0.0.0                0       Bound         52156
127.0.0.1        46006 127.0.0.1          46005 Established         52156

PS> Get-NetUDPEndpoint -OwningProcess 52156
(empty)

PS> netstat -ano | findstr "52156"
  TCP    127.0.0.1:14370        0.0.0.0:0              LISTENING       52156
  TCP    127.0.0.1:16800        0.0.0.0:0              LISTENING       52156
  TCP    127.0.0.1:46005        127.0.0.1:46006        ESTABLISHED     52156
  TCP    127.0.0.1:46006        127.0.0.1:46005        ESTABLISHED     52156
  TCP    [::1]:14370            [::]:0                 LISTENING       52156
  TCP    [::1]:16800            [::]:0                 LISTENING       52156
```

### 1.2 判读

| 端口 | 协议/地址 | 状态 | 用途（实测/源码核对） |
|---|---|---|---|
| 14370 | TCP，127.0.0.1 + ::1 | LISTEN | 浏览器扩展 WebSocket 桥（纯 WebSocket，不是 HTTP REST） |
| 16800 | TCP，127.0.0.1 + ::1 | LISTEN | Aria2 JSON-RPC 兼容门面，仅实现少量方法 |
| 46005 ↔ 46006 | TCP 回环 | ESTABLISHED/Bound | 进程内部回环对，非对外服务 |

无 UDP 监听。

---

## 2. 端口路径探测

探测命令统一为：

```powershell
curl.exe -sS -i --max-time 3 "http://127.0.0.1:<port><path>"
```

### 2.1 14370：所有路径都返回 426（纯 WebSocket 升级）

| 路径 | 方法 | 状态码 | 响应摘要 |
|---|---|---|---|
| `/` | GET | 426 Upgrade Required | 固定体 A |
| `/api` | GET | 426 Upgrade Required | 固定体 A |
| `/api/tasks` | GET | 426 Upgrade Required | 固定体 A |
| `/api/v1/tasks` | GET | 426 Upgrade Required | 固定体 A |
| `/api/task` | GET | 426 Upgrade Required | 固定体 A |
| `/tasks` | GET | 426 Upgrade Required | 固定体 A |
| `/status` | GET | 426 Upgrade Required | 固定体 A |
| `/health` | GET | 426 Upgrade Required | 固定体 A |
| `/docs` | GET | 426 Upgrade Required | 固定体 A |
| `/openapi.json` | GET | 426 Upgrade Required | 固定体 A |
| `/ws` | GET | 426 Upgrade Required | 固定体 A |
| `/websocket` | GET | 426 Upgrade Required | 固定体 A |
| `/` | POST | 405 Method Not Allowed | `Failed to open a WebSocket connection: unsupported HTTP method: POST.`（`Allow: GET`，体长 70） |
| `/api/tasks` | POST | 405 Method Not Allowed | 同上 |

固定体 A（长度 158 字节）：

```text
Failed to open a WebSocket connection: missing Connection header.
You cannot access a WebSocket server directly with a browser. You need a WebSocket client.
```

响应头包含 `HTTP/1.1 426 Upgrade Required` 与 `Upgrade: websocket`。**路径完全不参与路由**：该端口只做 WebSocket 升级，因此所有路径行为一致。

### 2.2 16800：所有常见路径 404

| 路径 | 方法 | 状态码 | 响应摘要 |
|---|---|---|---|
| `/` | GET | 404 Not Found | 空体，`Content-Length: 0` |
| `/api` | GET | 404 Not Found | 空体 |
| `/api/tasks` | GET | 404 Not Found | 空体 |
| `/api/v1/tasks` | GET | 404 Not Found | 空体 |
| `/api/task` | GET | 404 Not Found | 空体 |
| `/tasks` | GET | 404 Not Found | 空体 |
| `/status` | GET | 404 Not Found | 空体 |
| `/health` | GET | 404 Not Found | 空体 |
| `/docs` | GET | 404 Not Found | 空体 |
| `/openapi.json` | GET | 404 Not Found | 空体 |
| `/ws` | GET | 404 Not Found | 空体 |
| `/websocket` | GET | 404 Not Found | 空体 |
| `/` | POST | 404 Not Found | 空体（`Access-Control-Allow-Origin: *`） |
| `/` | OPTIONS | 200 OK | 空体（`Access-Control-Allow-Origin: *`） |

另有 `/api/download`、`/download`、`/add`、`/addTask`、`/newTask`、`/task`、`/v1`、`/api/v1`、`/version`、`/info`、`/ping`、`/extension`、`/browser`、`/api/status`、`/api/version`、`/api/downloads`、`/api/add`、`/rpc`、`/jsonrpc`（GET）、`/favicon.ico` 全部 404。

原始响应样例：

```text
HTTP/1.1 404 Not Found
Access-Control-Allow-Origin: *
Content-Length: 0
Connection: close
```

### 2.3 16800 实际是 Aria2 JSON-RPC 门面

`POST /jsonrpc` 有效，但只实现了极少数方法：

```text
=== POST /jsonrpc method=aria2.getVersion ===
HTTP/1.1 200 OK
Content-Type: application/json-rpc

{"jsonrpc": "2.0", "id": "probe", "result": {"version": "4.5.11", "enabledFeatures": ["HTTPS"]}}

=== POST /jsonrpc method=system.listMethods ===
HTTP/1.1 400 Bad Request
{"jsonrpc": "2.0", "id": "probe", "error": {"code": 1, "message": "No such method: system.listMethods"}}

=== POST /jsonrpc method=aria2.getGlobalStat ===
HTTP/1.1 400 Bad Request
{"jsonrpc": "2.0", "id": "probe", "error": {"code": 1, "message": "No such method: aria2.getGlobalStat"}}

=== POST /jsonrpc method=aria2.tellActive ===
HTTP/1.1 400 Bad Request
{"jsonrpc": "2.0", "id": "probe", "error": {"code": 1, "message": "No such method: aria2.tellActive"}}

=== POST /jsonrpc method=aria2.tellStatus ===
HTTP/1.1 400 Bad Request
{"jsonrpc": "2.0", "id": "probe", "error": {"code": 1, "message": "No such method: aria2.tellStatus"}}
```

| 方法 | 状态码 | 结果 |
|---|---|---|
| `aria2.getVersion` | 200 | `{"version":"4.5.11","enabledFeatures":["HTTPS"]}` |
| `system.listMethods` | 400 | No such method |
| `aria2.getGlobalStat` | 400 | No such method |
| `aria2.tellActive` | 400 | No such method |
| `aria2.tellStatus` | 400 | No such method |

**16800 无法读取任何任务进度/速度。**

---

## 3. WebSocket 协议与实测握手

### 3.1 连接与认证

- 地址：`ws://127.0.0.1:14370/`
- 令牌来源：`C:\Users\Microsoft\AppData\Local\GhostDownloader\UserConfig.json` 的 `Browser.BrowserExtensionPairToken`，本次为 `YtwT1lERwgq-d_B6BoCbFw`
- 候选端口：14370 / 24370 / 34370 / 44370 / 38370（当前使用 14370）
- 相关配置实测值：`Browser.Port=14370`、`Browser.EnableBrowserExtension=true`、`Browser.AutoSelectPort=true`；`Aria2Rpc.Port=16800`、`Aria2Rpc.Token=""`、`Aria2Rpc.Enabled=true`；`GeneralDownload.DownloadFolder="D:\\下载"`、`PreBlockNum=16`、`AutoSpeedUp=true`

发送：

```json
{"type": "hello", "protocolVersion": 2, "token": "YtwT1lERwgq-d_B6BoCbFw", "requestId": "probe-hello", "extensionVersion": "9.9.9", "installType": "probe"}
```

收到的原始应答：

```json
{"type": "hello_ack", "protocolVersion": 2, "appVersion": "4.5.11", "capabilities": {"taskSnapshots": true, "taskActions": ["toggle_pause", "cancel", "remove", "redownload", "open_file", "open_folder"]}}
```

随后发送：

```json
{"type": "subscribe_tasks"}
```

立即收到一帧全量 `task_snapshot`；之后下载过程中约每秒推送一帧。

### 3.2 任务快照字段

每帧结构为 `{"type":"task_snapshot","tasks":[...]}`，`tasks[]` 中每个任务对象字段：

| 字段 | 含义 | 实测值示例 |
|---|---|---|
| `taskId` | 任务 ID | `tsk_d6477d454c5b42898389fd36273dc89e` |
| `name` | 文件名 | `ghostprobe-20261122-022522.bin` |
| `status` | 状态（小写） | `running` / `completed` |
| `progress` | 进度，**0–100 百分比** | `13.89`、`30.33`、`100.0` |
| `receivedBytes` | 已完成字节 | `58261504` |
| `fileSize` | 总字节 | `419430400` |
| `speed` | 速度，**bytes/s** | `33302528` |
| `createdAt` | Unix 秒 | `1795285523` |
| `fileExt` | 扩展名 | `bin` |
| `packName` | 下载器类型 | `http` |

### 3.3 原始帧（≥5 帧，原文切片，每帧 ≤600 字节）

以下为原始 `task_snapshot` 行里 `tasks[]` 中**本任务对象**的原文切片（未改写、未重新序列化；整行还包含其他已有任务，故只摘录本任务对象）：

```json
{"taskId": "tsk_d6477d454c5b42898389fd36273dc89e", "name": "ghostprobe-20261122-022522.bin", "status": "running", "progress": 0.0, "receivedBytes": 0, "fileSize": 419430400, "speed": 0, "createdAt": 1795285523, "canPause": true, "canOpenFile": true, "canOpenFolder": true, "fileExt": "bin", "packName": "http"}
```
```json
{"taskId": "tsk_d6477d454c5b42898389fd36273dc89e", "name": "ghostprobe-20261122-022522.bin", "status": "running", "progress": 5.95, "receivedBytes": 24958976, "fileSize": 419430400, "speed": 24958976, "createdAt": 1795285523, "canPause": true, "canOpenFile": true, "canOpenFolder": true, "fileExt": "bin", "packName": "http"}
```
```json
{"taskId": "tsk_d6477d454c5b42898389fd36273dc89e", "name": "ghostprobe-20261122-022522.bin", "status": "running", "progress": 13.89, "receivedBytes": 58261504, "fileSize": 419430400, "speed": 33302528, "createdAt": 1795285523, "canPause": true, "canOpenFile": true, "canOpenFolder": true, "fileExt": "bin", "packName": "http"}
```
```json
{"taskId": "tsk_d6477d454c5b42898389fd36273dc89e", "name": "ghostprobe-20261122-022522.bin", "status": "running", "progress": 30.33, "receivedBytes": 127205376, "fileSize": 419430400, "speed": 34078720, "createdAt": 1795285523, "canPause": true, "canOpenFile": true, "canOpenFolder": true, "fileExt": "bin", "packName": "http"}
```
```json
{"taskId": "tsk_d6477d454c5b42898389fd36273dc89e", "name": "ghostprobe-20261122-022522.bin", "status": "running", "progress": 46.64, "receivedBytes": 195630080, "fileSize": 419430400, "speed": 34804736, "createdAt": 1795285523, "canPause": true, "canOpenFile": true, "canOpenFolder": true, "fileExt": "bin", "packName": "http"}
```
```json
{"taskId": "tsk_d6477d454c5b42898389fd36273dc89e", "name": "ghostprobe-20261122-022522.bin", "status": "running", "progress": 70.67, "receivedBytes": 296419328, "fileSize": 419430400, "speed": 50331648, "createdAt": 1795285523, "canPause": true, "canOpenFile": true, "canOpenFolder": true, "fileExt": "bin", "packName": "http"}
```
```json
{"taskId": "tsk_d6477d454c5b42898389fd36273dc89e", "name": "ghostprobe-20261122-022522.bin", "status": "completed", "progress": 100.0, "receivedBytes": 393300224, "fileSize": 419430400, "speed": 0, "createdAt": 1795285523, "canPause": true, "canOpenFile": true, "canOpenFolder": true, "fileExt": "bin", "packName": "http"}
```

对应原始行首时间戳依次为 `2026-11-22T02:25:23`、`02:25:24`、`02:25:25`、`02:25:27`、`02:25:29`、`02:25:31`、`02:25:34`；同一份 `run2-20261122-022522.raw.log` 共记录 13 帧 `task_snapshot`（`02:25:22`–`02:25:34`，相邻时间戳均为 1 秒），即 **约 1 Hz**。

### 3.4 触发真实下载（自动化方式）

URL 关联启动（`Start-Process Ghost-Downloader-3.exe -ArgumentList "http://127.0.0.1:18080/xxx.bin"`）实测只弹出一个**隐藏的「添加任务」窗口**（`IsWindowVisible=False`），没有创建任务、没有生成文件、`tasks.jsonl` 也不变——该路径需要 GUI 人工确认，不适合自动化。

改用 WebSocket `create_task`：

```json
{"type": "create_task", "requestId": "probe-create-20261122-022522", "source": "download", "title": "ghostprobe-20261122-022522.bin", "draft": false, "payload": {"url": "http://127.0.0.1:18080/ghostprobe-20261122-022522.bin", "filename": "ghostprobe-20261122-022522.bin", "size": 419430400, "supportsRange": true, "headers": {}, "path": "D:\\下载", "preBlockNum": 8}}
```

原始应答：

```json
{"type": "create_task_result", "requestId": "probe-create-20261122-022522", "status": "created", "taskId": "tsk_d6477d454c5b42898389fd36273dc89e"}
```

`create_task` 创建的是**真实下载任务**：文件真实写入 `D:\下载`，出现在 `tasks.jsonl` 中，完成后 SHA256 与源文件一致。

---

## 4. 真实限速下载采样

### 4.1 测试环境

- 限速源：`py -3 .probe\slow_http.py .probe\big.bin 18080 4`（每连接 4 MB/s）
- 文件：`D:\Daozai_fork\.probe\big.bin`，419,430,400 字节（400 MiB）
- SHA256：`6ED5E85372E488807486F4446E2A3A501D319BE812E969E3DE426DB798CC5704`
- 服务端监听：`127.0.0.1:18080`，PID 13860

### 4.2 Run A（preBlockNum=8，8 连接，约 11 秒完成）

任务：`tsk_d6477d454c5b42898389fd36273dc89e`，文件名 `ghostprobe-20261122-022522.bin`

| 时间 | WS `status` | WS `progress`(%) | WS `receivedBytes` | WS `speed`(bytes/s) | `tasks.jsonl` mtime | 目标文件大小 |
|---|---|---|---|---|---|---|
| 02:25:23 | running | 0.0 | 0 | 0 | 02:25:23.386 | 419430400 |
| 02:25:25 | running | 13.89 | 58261504 | 33302528 | 02:25:23.386 | 419430400 |
| 02:25:27 | running | 30.33 | 127205376 | 34078720 | 02:25:23.386 | 419430400 |
| 02:25:29 | running | 46.64 | 195630080 | 34804736 | 02:25:23.386 | 419430400 |
| 02:25:31 | running | 70.67 | 296419328 | 50331648 | 02:25:23.386 | 419430400 |
| 02:25:33 | running | 93.77 | 393300224 | 44189952 | 02:25:23.386 | 419430400 |
| 02:25:35 | completed | 100.0 | 393300224 | 0 | 02:25:34.412 | 419430400 |

- 文件从第 1 次采样起就是 `419430400` 字节（Ghost 预分配），**文件大小不能当进度**。
- `tasks.jsonl` 的 mtime 在下载全程停留在创建时刻 `02:25:23.386`，直到完成才变为 `02:25:34.412`。
- 完成时 `receivedBytes` 停在 `393300224`（约为总大小的 93.77%），但文件实际完整：下载后 SHA256 与源文件完全一致，`match=True`。

### 4.3 Run B（preBlockNum=2，2 连接，约 18 秒完成，同时采 16800）

任务：`tsk_ae0ba11388e04725bfb0305582b8aaf3`，文件名 `ghostprobe-20261122-022848.bin`

| 时间 | WS `status` | WS `progress`(%) | WS `receivedBytes` | WS `speed`(bytes/s) | `tasks.jsonl` mtime | 目标文件大小 | 16800 `tellStatus` |
|---|---|---|---|---|---|---|---|
| 02:28:48 | running | 0.0 | 0 | 0 | 02:25:34.412（旧） | 419430400 | 400 No such method |
| 02:28:50 | running | 3.86 | 16187392 | 8403456 | 02:28:48.646 | 419430400 | 400 No such method |
| 02:28:52 | running | 7.89 | 33095680 | 8519680 | 02:28:48.646 | 419430400 | 400 No such method |
| 02:28:54 | running | 15.58 | 65339392 | 23986176 | 02:28:48.646 | 419430400 | 400 No such method |
| 02:28:56 | running | 27.7 | 116179712 | 25574912 | 02:28:48.646 | 419430400 | 400 No such method |
| 02:28:58 | running | 39.77 | 166821888 | 25264128 | 02:28:48.646 | 419430400 | 400 No such method |
| 02:29:00 | running | 55.81 | 234094592 | 41222144 | 02:28:48.646 | 419430400 | 400 No such method |
| 02:29:02 | running | 76.11 | 319225856 | 42598400 | 02:28:48.646 | 419430400 | 400 No such method |
| 02:29:04 | running | 96.3 | 403918592 | 42028800 | 02:28:48.646 | 419430400 | 400 No such method |
| 02:29:06 | completed | 100.0 | 403918592 | 0 | 02:29:05.365 | 419430400 | 400 No such method |

每次采样 `aria2.getVersion` 恒为 200，`aria2.tellStatus` 恒为 400 `No such method`。

### 4.4 `tasks.jsonl` 里的同一任务（下载中）

Run A 进行中（02:25:27）该任务在 `tasks.jsonl` 的 `steps[0]` 原文：

```json
{"stepIndex": 1, "fileIndex": null, "stepId": "stp_a951c745d2b542a49a035fe1e814d8dd", "status": "RUNNING", "progress": 0.0, "receivedBytes": 0, "speed": 0, "url": "http://127.0.0.1:18080/ghostprobe-20261122-022522.bin", "fileSize": 419430400, "headers": {}, "clientProfile": "", "userAgent": "", "subworkerCount": 8, "canUseRangeRequests": true, "httpByteOffset": 0, "lastModified": "", "filePath": "", "isAccelerated": false, "type": "http_pack.task.HttpTaskStep"}
```

同一任务完成后的 `steps[0]`：

```json
{"stepIndex": 1, "fileIndex": null, "stepId": "stp_a951c745d2b542a49a035fe1e814d8dd", "status": "COMPLETED", "progress": 100, "receivedBytes": 393300224, "speed": 0, "url": "http://127.0.0.1:18080/ghostprobe-20261122-022522.bin", "fileSize": 419430400, "headers": {}, "clientProfile": "", "userAgent": "", "subworkerCount": 8, "canUseRangeRequests": true, "httpByteOffset": 0, "lastModified": "", "filePath": "", "isAccelerated": false, "type": "http_pack.task.HttpTaskStep"}
```

结论：下载全程 `progress` / `receivedBytes` / `speed` 恒为 `0.0` / `0` / `0`，只在完成时 `progress` 跳 `100`、`receivedBytes` 才有值。`tasks.jsonl` 没有顶层 `progress` / `speed` 字段。

---

## 5. 结论与复用方式

### 5.1 能力矩阵

| 通道 | 能否拿到实时进度 | 能否拿到实时速度 | 说明 |
|---|---|---|---|
| **14370 WebSocket + `subscribe_tasks`** | ✅ `progress` 0–100、`receivedBytes` | ✅ `speed`（bytes/s） | 唯一可用通道，约 1 Hz 推送全量 `task_snapshot` |
| 16800 Aria2 JSON-RPC | ❌ | ❌ | 运行时实测只有 `getVersion` 可用；源码/字符串显示另实现 `getGlobalOption` / `addUri`，但 `tellStatus`、`tellActive`、`getGlobalStat` 均返回 `No such method` |
| `tasks.jsonl` | ❌（仅完成时跳 100） | ❌（恒 0） | 下载中 `steps[].progress=0.0`、`receivedBytes=0`、`speed=0` |
| 目标文件大小 | ❌ | ❌ | Ghost 预分配，创建瞬间即等于 `fileSize` |

### 5.2 可直接复用的请求

1. 从 `%LOCALAPPDATA%\GhostDownloader\UserConfig.json` 读取 `Browser.Port` 与 `Browser.BrowserExtensionPairToken`（候选端口 14370/24370/34370/44370/38370）。
2. 连接 `ws://127.0.0.1:<port>/`。
3. 发送：

```json
{"type":"hello","protocolVersion":2,"token":"<BrowserExtensionPairToken>"}
```

4. 收到 `hello_ack` 后发送：

```json
{"type":"subscribe_tasks"}
```

5. 读取每帧 `task_snapshot.tasks[]`，按 `taskId` 或 `name` 过滤目标任务，字段路径：

```text
tasks[].taskId
tasks[].name
tasks[].status          # running / paused / completed / failed ...
tasks[].progress        # 0..100（百分比，不是 0..1）
tasks[].receivedBytes   # 已完成字节
tasks[].fileSize        # 总字节
tasks[].speed           # bytes/s
```

Python 最小示例：

```python
import asyncio, json, websockets

async def main():
    async with websockets.connect("ws://127.0.0.1:14370/") as ws:
        await ws.send(json.dumps({
            "type": "hello", "protocolVersion": 2,
            "token": "YtwT1lERwgq-d_B6BoCbFw"}))
        print(await ws.recv())          # hello_ack
        await ws.send(json.dumps({"type": "subscribe_tasks"}))
        while True:
            msg = json.loads(await ws.recv())
            if msg.get("type") != "task_snapshot":
                continue
            for t in msg["tasks"]:
                print(t["taskId"], t["status"], t["progress"],
                      t["receivedBytes"], t["fileSize"], t["speed"])

asyncio.run(main())
```

### 5.3 量纲与单位（实测确认）

- `progress` 是 **0–100 百分比**。验证：`58,261,504 / 419,430,400 × 100 = 13.89`，与同帧 `progress=13.89` 完全吻合；最终帧为 `100.0`。
- `speed` 是 **bytes/s**。验证：02:25:24→02:25:25 的 `receivedBytes` 增量为 `58,261,504 − 24,958,976 = 33,302,528`，与后一帧（02:25:25）的 `speed=33,302,528` 一致。8 连接 × 4 MB/s ≈ 33.3 MB/s；2 连接时 `8,403,456 ≈ 2 × 4 MiB/s`，同样吻合。
- `receivedBytes` 是**已完成字节**，但完成帧可能小于 `fileSize`（本次为 393,300,224 / 403,918,592，而文件实际完整且 SHA256 一致）。判断完成应使用 `status == "completed"`，不要用 `receivedBytes == fileSize`。

### 5.4 刷新频率与注意事项

- **刷新频率：约 1 秒 1 帧**。Run A 的 `task_snapshot` 共 13 帧、相邻时间戳均为 1 秒；下载中每帧的 `progress` / `receivedBytes` / `speed` 都在变化。
- 每帧是**全部任务**的完整列表（包含已完成的历史任务），需要按 `taskId`/`name` 过滤。
- 内容相同的快照会被去重跳过；空闲时不一定每秒都有帧。
- `protocolVersion` 必须为 `2`，否则服务端按协议不匹配断开；`token` 必须是 UserConfig 里的 `BrowserExtensionPairToken`。
- `subscribe_tasks` 后会**立即**收到一帧当前快照，可用于获取历史任务状态。

---

## 6. 清理记录

### 6.1 删除测试任务记录

用 WebSocket `task_action` 的 `remove` 动作让 Ghost 自己删除（`shouldDeleteFiles=False`，不删文件）：

```json
{"type":"task_action","requestId":"cleanup-tsk_d6477d454c5b42898389fd36273dc89e","action":"remove","taskId":"tsk_d6477d454c5b42898389fd36273dc89e"}
→ {"type":"task_action_result","requestId":"cleanup-tsk_d6477d454c5b42898389fd36273dc89e","ok":true}

{"type":"task_action","requestId":"cleanup-tsk_ae0ba11388e04725bfb0305582b8aaf3","action":"remove","taskId":"tsk_ae0ba11388e04725bfb0305582b8aaf3"}
→ {"type":"task_action_result","requestId":"cleanup-tsk_ae0ba11388e04725bfb0305582b8aaf3","ok":true}
```

清理前后 `tasks.jsonl`：

```text
[BEFORE] bytes=70426 sha256=280f6b3d57ee3d53 lines=24 crlf=24 lf=24
  line 23: taskId=tsk_d6477d454c5b42898389fd36273dc89e name=ghostprobe-20261122-022522.bin status=COMPLETED
  line 24: taskId=tsk_ae0ba11388e04725bfb0305582b8aaf3 name=ghostprobe-20261122-022848.bin status=COMPLETED
[AFTER] bytes=68633 sha256=01c7c5abe7fb5b3c lines=22 crlf=22 lf=22
```

只删除了上述两条 ghostprobe 记录，其余 22 条记录保持原样，文件仍为 UTF-8 + CRLF。复查：

```text
ghostprobe/tsk probe records: none
```

### 6.2 删除测试下载文件

| 文件 | 大小 | SHA256 | 处理 |
|---|---|---|---|
| `D:\下载\ghostprobe-20261122-022522.bin` | 419430400 | `6ED5E853...798CC5704`（与源文件一致） | 已删除 |
| `D:\下载\ghostprobe-20261122-022848.bin` | 419430400 | `6ED5E853...798CC5704`（与源文件一致） | 已删除 |

复查：`D:\下载` 下无 `ghostprobe-*.bin`。

### 6.3 停止限速服务

```text
OWNER pid=13860 name=python path=C:\Users\Microsoft\AppData\Local\Python\pythoncore-3.14-64\python.exe
SUCCESS: The process with PID 13860 has been terminated.
---- after ----
18080 free
```

### 6.4 未触碰项

- Ghost 主进程 52156 仍在运行：`Get-Process -Id 52156` → `Ghost-Downloader-3`，启动时间 `2026/11/21 23:31:31`。
- 未修改 `D:\Daozai_fork\src` 下任何文件。
- 未触碰任何非 `ghostprobe-` 的任务记录与下载文件。

---

## 7. 原始证据文件

| 文件 | 内容 |
|---|---|
| `D:\Daozai_fork\.probe\run2-20261122-022522.jsonl` | Run A 每 2 秒采样记录（WS 字段、`tasks.jsonl` mtime、任务条目、文件大小） |
| `D:\Daozai_fork\.probe\run2-20261122-022522.raw.log` | Run A 原始 WebSocket 帧（13 帧 `task_snapshot`，每行带时间戳） |
| `D:\Daozai_fork\.probe\run3-20261122-022848.jsonl` | Run B 每 2 秒采样记录（含 16800 `getVersion`/`tellStatus` 响应） |
| `D:\Daozai_fork\.probe\sample_run3.py` | Run B 采样脚本（WS + 16800 + 文件/JSONL 同步采样） |
| `D:\Daozai_fork\.probe\cleanup_tasks.py` | 清理脚本（`task_action remove`） |
| `D:\Daozai_fork\.probe\extract_frames.py` | 从 raw.log 提取本任务原始帧切片 |

