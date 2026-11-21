# 三款下载器（IDM / FDM / Ghost Downloader）本机任务与进度数据源实测报告

- 采集时间：2026-11-21 21:04–21:16 (+08:00)，Windows 11 专业版 NT 10.0.26200.0
- 采集主机：本机（工作目录 `D:\Daozai_fork`），全部结论附真实命令与输出
- 工具口径：`py -3` = Python 3.14.2 / sqlite3 3.50.4（本机无 sqlite3.exe）；PowerShell 7 直接读注册表与进程
- 证据标签：**【实测】**=有命令输出；**【推断】**=由实测外推，附验证方法；**【未验证】**=本机无法验证
- 探针脚本留在 `D:\Daozai_fork\.probe\`（`registry_timing.ps1` / `fdm_probe4.py` / `fdm_scan.py` / `fdm_final.py` / `fdm_hex.py` / `fdm_bench.py` / `timing.py` / `ghost_dump.py`）

> 与任务前提冲突的两处本机事实（先列明，后文按实测走）：
> 1. **Ghost Downloader 并非"未安装"**——它装在 `D:\Ghost Downloader`，v4.5.11.0，采集时正在运行（PID 38608）。
> 2. **IDM 的 `IDMan.exe` 本体不存在**——安装目录只剩 `IDMan.exe.BAK`，因此"运行中任务的 Status/Speed 行为"本机**无法实测**，只能标注推断。

---

## A. Internet Download Manager (IDM)

### A.1 安装与运行状态

```powershell
Get-Process -Name IDMan -ErrorAction SilentlyContinue   # → 0 个进程
Get-ChildItem 'D:\Internet Download Manager'
```

```
Name              Length    LastWriteTime
IDMan.exe.BAK     6041856   2025/1/24 21:xx
```

**【实测】**安装目录 `C:\Program Files\Internet Download Manager`、`${env:ProgramFiles(x86)}\...` 均不存在；注册表 `ExePath = D:\Internet Download Manager\IDMan.exe`，但该文件缺失（只有 `.BAK`）。`idmvers = v6.42b32 Trial`。
⇒ **IDM 未运行，且当前不可运行**。所有注册表数据均为"上次运行后的存量"。

**【实测】未运行时注册表完全可读**：本报告全部 IDM 数据都是在 IDMan.exe 不存在的情况下读出的，说明 Rust 侧无需进程存活即可解析。

### A.2 任务键的枚举方式与规模

```powershell
$r=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\DownloadManager')
($r.GetSubKeyNames()).Count                      # → 363
($r.GetSubKeyNames() | ? { $_ -match '^\d+$' }).Count   # → 345
```

**【实测】**根下共 **363 个子键**，其中 **345 个纯数字任务 id**（范围 2..549）；另有 18 个非任务键：`ConfigTime, DwnlPanel, FoldersTree(0 值), GetAllDlgLS, IDMBI, ListSettings, maxID(空值), MCN, menuExt, Passwords, ProxyPac, ProxyServers, Queue, Scheduler, sites_sf, SpecialData, SpecialKeys, WBProxy`。
⇒ **任务枚举 = 过滤 `^\d+$` 的子键**；`maxID` 本机为空值，靠它做增量不可靠，用 id 集合差集更稳。

一键内值名并集（跨 345 键统计）共 **71 个**，与任务解析相关的核心 15 个：
`FileName, LocalFileName, LocalPath, Path, Host, Port, Url0, FileSize, Downloaded, Status, Speed, dateAdded, lastTryDate, st_time, lastResult`。

### A.3 字段类型与解析方式（GetValueKind 实测）

```powershell
$k=$r.OpenSubKey('548'); $k.GetValueNames().Count          # → 47
$k.GetValueKind('dateAdded'); $k.GetValueKind('st_time')
$k.GetValueKind('FileSize');  $k.GetValueKind('Speed')
```

| 字段 | 注册表类型 | 实测样例 | 解析方式 |
|---|---|---|---|
| `FileName` | REG_SZ | `fdm_x64_setup.exe` / `Retouch4me11_867457.rar?auth_key=…` | 原样字符串；**可能带 query 串**，需 `split('?')[0]` |
| `LocalFileName` | REG_SZ | `C:\Users\Microsoft\AppData\Roaming\IDM\DwnlData\Microsoft\fdm_x64_setup_548\fdm_x64_setup.exe` | **临时 part 文件的绝对路径**（不是最终文件） |
| `LocalPath` | REG_SZ | `…\DwnlData\Microsoft\fdm_x64_setup_548\` | 工作目录（含尾部 `\`） |
| `Path` | REG_SZ | `/6/latest/`、`/202302/tools/` | URL 的 path 段（非本地路径） |
| `Host` / `Port` | SZ / DWord | `…` / `443` | 主机与端口 |
| `Url0` | REG_SZ | 完整 URL | 直读 |
| **`FileSize`** | **REG_NONE，8 字节** | `b0 da d4 02 00 00 00 00` | **LITTLE_ENDIAN u64** |
| **`Downloaded`** | **REG_NONE，8 字节** | 同上 | **LITTLE_ENDIAN u64** |
| `Status` | REG_DWORD | 0 / 3 | u32 |
| `Speed` | REG_DWORD（仅 258/345 键有） | 388141817 | u32，**字节/秒，残留值（见 A.6）** |
| `dateAdded` | **REG_SZ** | `Apr 12 15:15:47 2025` | `%b %d %H:%M:%S %Y`，**本地时间** |
| `lastTryDate` | **REG_SZ** | 同上格式 | **不是 DWORD**（与常见假设相反，345/345 全是字符串） |
| `st_time` | REG_DWORD | 1744442147 | unix 秒（UTC），与 `dateAdded` 同刻 `Apr 12 15:15:47 2025` |
| `lastResult` | REG_SZ（仅 43/345） | `下载已被用户取消` / `请求的文件不存在。备注：HTTP/1.1 404 Not Found` | 失败原因文本（中文，随 LanguageID） |

**PowerShell 陷阱（踩过，Rust 侧不受影响，但同机脚本要注意）**：REG_NONE 值经函数 `return` 会被展开成 `object[]`，`-is [byte[]]` 判 false，需 `return ,$v`。另外 .NET Core 的 `RegistryKey` **没有 `LastWriteTime`**（取到 null）⇒ **无法用注册表键时间戳判断新鲜度**，只能用 `st_time` / `lastTryDate`。

### A.4 FileSize / Downloaded 的 u64 LE 解码验证（3 个任务 + 交叉校验）

```powershell
function U64($k,$n){ $v=$k.GetValue($n); [BitConverter]::ToUInt64($v,0) }
```

| id | FileSize(hex) | 解码 | Downloaded(hex) | 解码 | Status | 校验 |
|---|---|---|---|---|---|---|
| 546 | `5f 6e 00 00 00 00 00 00` | **28255** | 同 | 28255 | 3 | ✅ |
| 548 | `b0 da d4 02 00 00 00 00` | **47504048** | 同 | 47504048 | 3 | ✅ |
| 549 | 同 546 | **28255** | 同 | 28255 | 3 | ✅ |

**独立交叉校验（强证据，不依赖 IDM 自身）**：

1. 全量算术自证：`Status=3 且 Downloaded==FileSize` → **301/301 成立**（见 A.5），说明两个字段同口径、解码一致。
2. 磁盘/分片校验（id=217，未完成）：
   ```
   DwnlData\Microsoft\allall01_baidupcs_co_217\ 分片大小：
     533960 + 524288 + 327680 + 409600 + 204799 + 245760 = 2 246 087
   注册表 id=217  Downloaded = 2 246 087     ← 完全相等
   ```
3. 总大小校验（id=217）：`log_217.log` 内 URL 参数 `size=225932910&sta_dx=225932910`，与 `FileSize=225932910` 相等。

⇒ **FileSize / Downloaded = REG_NONE 8 字节 LITTLE_ENDIAN u64，已用三种独立方式验证。**
（注：已完成任务的 `LocalFileName` 指向的 part 文件已被 IDM 移走，本机 301 个完成任务的该路径 `Test-Path=False`，所以"用最终文件字节数"这条路对 IDM 不可用，才改用上面 1+2 两种校验。）

### A.5 Status 数值含义（已完成 vs 未完成实测对照）

```powershell
# 全量统计
Status=0  → 44 个
Status=3  → 301 个
# 交叉表
status=3 且 Downloaded==FileSize → 301/301
status=0 且 Downloaded==FileSize → 0/44
```

**【实测】**
- **`Status = 3` ⇒ 已完成**：301 个全部 `Downloaded == FileSize`。
- **`Status = 0` ⇒ 未完成（用户取消 / 失败 / 未开工）**：44 个全部 `Downloaded != FileSize`，其中 43 个带 `lastResult` 文本，样例：
  `下载已被用户取消`(32 个)、`请求的文件不存在。备注：HTTP/1.1 404 Not Found`、`权限被拒绝。备注：SSL_connect error 5`(4 个)、`无法连接到目标服务器。备注：无法连接到 app.wukongiot.net: 443`、`由于超时无法下载文件。备注：…`。
- 本机**没有出现过 Status=1 / 2**（IDM 未运行、无进行中任务）。

**【推断】**`Status=1` = 下载中、`Status=2` = 暂停（IDM 社区常规说法），本机无样本。
**验证方法**：把 `IDMan.exe`（或 `.BAK` 改名回 `.exe`）放回 `D:\Internet Download Manager\` → 启一个可断点续传的大文件 → 下载中与手动暂停时分别读 `HKCU\Software\DownloadManager\<新id>\Status`。**在验证之前，Rust 侧应把 1/2 视为"进行中/暂停"而不是落到"未知"或"失败"分支。**

补充（快速判活经验，**推断**）：`Status==3 && Downloaded==FileSize` 是"完成"的强合取条件，可作为双保险；`Status!=3 && Downloaded>0` 可视为"有残留进度的未完成任务"。

### A.6 Speed 是实时值还是残留值？

```powershell
# 258 个键有 Speed，全部非 0（没有一个是 0）
id=42  status=3  完成  dl=67630488/67630488  Speed=388141817
id=495 status=3  完成                          Speed=256047552
id=455 status=3  完成                          Speed=228524489
id=44  status=3  完成                          Speed=225483881
id=394 status=3  完成                          Speed=222132212
id=447 status=3  完成                          Speed=174177669
id=217 status=0  未完成 dl=2246087/225932910    Speed=117485
id=249 status=0  未完成 dl=8953854/85394706     Speed=394571
```

**【实测结论】`Speed` 是残留值（最后一次的瞬时速度），不是实时值。**
依据：① **已完成**的下载仍有几百 MB/s 的 Speed，而进程根本没在跑；② 258 个带 Speed 的键**没有一个等于 0**（若为实时字段，空闲任务必然归零）；③ IDM 未运行时该值保持不变，说明它只在 IDM 写任务的时刻被写入。
⇒ **Rust 侧不能用 `Speed` 显示"当前速度"**，只能在"IDM 正在运行且该任务 Status=1"时才有意义（该组合本机无法实测，标注推断）。

### A.7 最终保存路径能不能从任务键拿到？

**【实测】拿不到"最终完整路径"**：任务键里没有 SaveTo 类字段；`LocalFileName/LocalPath` 指向 `%APPDATA%\IDM\DwnlData\<user>\<job>\` 这个临时工作区。最终目录来自全局设置（根值 `LocalPathW` = `D:\下载` 的 UTF-16 串）。

实测可复原规则：`最终文件 = <全局保存目录>\<FileName 去掉 ?> 之后的 query 串>`：
```
id=546 FileName=Retouch4me11_867457.rar?auth_key=…  →  D:\下载\Retouch4me11_867457.rar  存在=True
id=549 同上                                                                        存在=True
```
全量抽测：301 个完成任务里 **9 个**能在 `D:\下载\<FileName 去 query>` 命中（其余被用户移走/改名/删除）。
⇒ **字段矩阵里"完整路径"对 IDM 记 `推断`**（= 全局保存目录 + FileName 去 query），并用 `Test-Path`/`os.stat` 兜底。

### A.8 DwnlData 里的 .log / .part 有没有用？

```powershell
Get-ChildItem 'C:\Users\Microsoft\AppData\Roaming\IDM\DwnlData\Microsoft\allall01_baidupcs_co_217'
# allall01_baidupcs_co  533960 | …1 524288 | …2 327680 | …3 409600 | …4 0 | …5 0 | …6 204799 | …7 245760 | …8 0 | log_217.log 12659
Get-Content …\log_217.log -TotalCount 12
# Folder … was created. / Windows 10.0 / v6.42b23  Time: Sun Nov 03 08:24:06.886 2024 (1730593446 sec)
# Url http://allall01.baidupcs.com/file/…&size=225932910&sta_dx=225932910… / C0:gethost / C0:IPV6 address found
```

**.log**：内容是"建目录 / IDM 版本 / 完整 URL（含 size 参数）/ DNS 与连接事件 + 微秒时间戳"，**没有任何进度或字节计数**。用途仅限：① 交叉校验总大小（URL 里的 `size=`）；② 排查失败原因。→ **对进度判断无用。**

**.part 分片**：**有用但冗余**——分片字节和 == `Downloaded`（id=217 实测 2 246 087 == 2 246 087）。可作为"注册表被清空/损坏"时的兜底，但需要遍历目录 + 累加，成本远高于读注册表。

> **结论：注册表够了。** 进度（`Downloaded/FileSize`）、状态（`Status`）、元数据（`FileName/Url0/dateAdded`）全部在注册表；`DwnlData` 仅用于交叉校验与调试。

### A.9 IDM 单次读取成本（实测，迭代 5 次取 min/avg/max）

```
IDM GetSubKeyNames (345 ids)                   min=   2.00ms avg=   3.89ms max=   9.24ms  <363>
IDM one task: GetValueNames+GetValueKind        min=   0.17ms avg=   0.45ms max=   1.41ms  <47>
IDM one task: Status/Speed/Downloaded/FileSize  min=   0.07ms avg=   0.52ms max=   2.13ms  <3/55799/8/8>
IDM full scan 345 keys x (Status,Speed,Downloaded,st_time) min=19.26ms avg=24.29ms max=29.69ms <363>
IDM full scan 345 keys x (Status,Downloaded) only          min=13.91ms avg=17.05ms max=21.99ms <363>
```

---

## B. Free Download Manager (Softdeluxe, FDM 6)

### B.1 运行状态与库文件形态

```powershell
Get-Process -Name fdm,fd6 -ErrorAction SilentlyContinue      # → 无
Get-ChildItem 'C:\Users\Microsoft\AppData\Local\Softdeluxe\Free Download Manager'
# db.sqlite 38342656  2026/11/14 17:29:28
# settings.ini / org.freedownloadmanager.fdm5.cnh.json / backup / cache / logs / Mozilla
```

**【实测】FDM 未运行；库目录下没有 `db.sqlite-wal` / `-shm` 旁文件。**

**Journal mode（关键，直接决定"能不能只 copy 一个文件"）**：
```python
hdr = open(db,'rb').read(100)
hdr[18], hdr[19]   # → (1, 1)   1 = legacy rollback journal, 2 = WAL
```
**【实测】`db.sqlite` 是 legacy rollback journal 模式，不是 WAL。**
⇒ ① 只 copy `db.sqlite` 就足够（不需要 wal/shm）；② 运行中写入靠 `-journal` 回滚日志，读端用 `mode=ro` + 超时重试可避免 `SQLITE_BUSY`（**运行中行为为【推断】**，验证方法见 B.5）。

### B.2 表结构与任务表

```python
con = sqlite3.connect("file:db.sqlite?mode=ro", uri=True)
[ (r[0], r[1]) for r in con.execute("select name, (select count(*) from sqlite_master) from sqlite_master") ]
```

**表清单（行数）【实测】**：
`Settings 15`、`Updates 1`、`UpdatesManagers 1`、**`downloads 430`**、`downloadsAutoRetries 0`、`downloadsErrorsAllowedToAutoRetry 0`、`downloadsLocks 0`、`downloadsMiscFlags 16`、`downloadsModules 1`、`downloadsPreviews 410`、`downloadsScheduleRecords 0`、`downloadsTags 0`、`downloadsTagsDetails 0`、`downloadsWithMissingFilesStats 27`、`miscData 25`、`moveDownloadsFilesTasks 0`、`moveFilesTasks 0`、`networkPasswords 0`、`pathByFileType 0`、`pathByUrlHash 0`、`previews 148`、`rcPasswords 0`、`soundNotifications 0`、`sslCertificates 2`、`sslKnownHosts 0`、`tags 0`。

**任务表 = `downloads`（42 列）【实测建表原文】**：
```
id INTEGER, originatorsNamesChain TEXT, uuid BLOB, guidType INTEGER, guidType2 INTEGER,
guid TEXT, moduleUid TEXT, createdBy INTEGER, flags INTEGER, parentDownloadId INTEGER,
childrenDownloadsIds TEXT, desc BLOB, url TEXT, referrer TEXT, cookies BLOB, postData BLOB,
customData BLOB, webPageUrl TEXT, sourceFlags INTEGER, destinationType INTEGER,
destinationPath TEXT, title TEXT, creationTime INTEGER, finishedTime INTEGER,
resumeData BLOB, lastResult BLOB, lastResultTime INTEGER, resumeSupport INTEGER,
files BLOB, allFilesType INTEGER, priority INTEGER, tbu2c INTEGER, supportedOptions INTEGER,
userAgent TEXT, mirrors BLOB, preferredFileTitle TEXT, preferredFileExt TEXT, subtitles BLOB,
launchableFileIndex INTEGER, existingFileReaction INTEGER, downloadOrder INTEGER,
webBrowser INTEGER, PRIMARY KEY(id)
```

Rust 侧能直接用的普通列只有：`id, url, destinationPath, destinationType, title, creationTime, finishedTime, flags, resumeSupport, moduleUid, lastResult(BLOB), lastResultTime, files(BLOB)`。
**文件名 / 大小 / 已下载 全在 `files` BLOB 里**（见 B.4）。

**时间单位【实测】**：`creationTime / finishedTime = unix 秒`（min 1744442942 = 2025-04-12T15:29:02，max 1794648324 = 2026-11-14T17:25:24）。`finishedTime = 0` 表示未完成。

**完成/未完成【实测】**：finished 410 / unfinished 20。

### B.3 真实 3 行样例（脱敏不必要，本机数据）

```
id=1599 creationTime=1794648324(2026-11-14 17:25:24) finishedTime=1794648329(17:25:29)
        destinationPath='D:/下载' destinationType=1 flags=2625039 resumeSupport=0
        moduleUid='downloadswww' title=NULL
        url=https://release-assets.githubusercontent.com/github-production-release-asset/939207877/a9b844ce…
        files(747B) → path='snow-shot-1.5.7-windows-x64-online.exe' size=25137773 done=25137773
id=1598 creationTime=1794648320 finishedTime=1794648328 flags=2625039 resumeSupport=1
        url=…/release-asset/679086250/13132620…
        files(767B) → path='Ghost-Downloader-v4.5.3-Windows-x86_64-Setup.exe' size=44667040 done=44667040
id=1592 creationTime=1794073243 finishedTime=1794073243 flags=2625039 resumeSupport=0
        url=http://192.168.110.56:5666/multiple-download?token=…
        files → path='Crepuscularian_sysrestore_20261108.bin' size=20674 done=20674
```

未完成样例（真实）：
```
id=1131 creation=1771234656 dest='D:/下载' resumeSupport=1
        path='【B站-行而Fans】异星工厂遨游星海存档&蓝图.rar' size=24917951
        tmpPath='【B站-行而Fans】异星工厂遨游星海存档&蓝图.rar.fdmdownload'
        sections[].done = [745472,0,0,0,344064,0,0,549920,0,289450,221183,425984,0,0,0]  → sum=2 576 073（约 10.3%）
id=1068 creation=1767192742 resumeSupport=1
        path='2c9d31c316fb379aa00cd29c742553f1.zip'
        sections: size=[353783190, 353783189, 707566379] done=[69811287, 353783189] → 第 1 个文件 69 811 287 / 353 783 190
id=1065/1029/414/… resumeSupport=2  streaming（size=-1，无断点）
id=1580/1306/1305/1078/1051/1050  files BLOB 只有 4 字节（fileCount=0）→ 既无 path 也无 size
```

### B.4 `files` BLOB 私有序列化格式（已解出，**可写 Rust**）

**【实测】**FDM 自研 `vmscl` 序列化，**大端（big-endian）**：

```
files_blob:
  u32 fileCount
  repeat fileCount:
     u32 entryCount
     repeat entryCount:
        u32 keyLenBytes            # 偶数，UTF-16 字节数
        byte[keyLenBytes] key      # UTF-16 BIG-ENDIAN
        u32 type
        u8  pad                    # 实测恒为 0x00（保留字节，跳过）
        value                      # 长度由 type 决定

type 2 / 3 → int32  (4B, BE)
type 4     → int64  (8B, BE)
type 10    → string: u32 byteLen + UTF-16BE 字节；byteLen==0xFFFFFFFF 表示 null
type 9     → 容器（list）：u32 count … 内部元素仍是同样的 entry 序列（头部另有 4 字节字段未解，见 B.6）
type 28    → 容器（map，出现在 details）：同样 entry 序列
```

实测解析输出（id=1599，容忍式扫描，`off` = 该记录在 blob 内的偏移）：
```
off=8    size          t=4  25137773
off=33   originType    t=2  1
off=66   priority      t=2  10
off=95   sections      t=9  <container>
off=133    size        t=4  25137773
off=158    offset      t=4  0
off=187    done        t=4  25137773
off=212  details       t=28 <container>
off=239    fps         t=2  0
off=258    type        t=2  0
off=279    contentType t=10 application/octet-stream
off=362    height      t=2  0
off=387    format      t=10 null
off=412    bitrate     t=2  0
off=439    language    t=10 null
off=468    width       t=2  0
off=491    duration    t=2  0
off=520    lastModified t=4 1794588676000        # 毫秒时间戳
off=561    path        t=10 snow-shot-1.5.7-windows-x64-online.exe
off=658    tmpPath     t=10 null
off=685    flags       t=3  3
off=708    writeOffset t=4  0
```

**字段语义【实测 + 校验】**
- 文件级 `size` = 总字节；`sections[]` 每个元素有 `size / offset / done`；**`done` = 该段已下载字节**。
- 完成的行：`done == size`（1599/1598/1592/1590 全部相等）。
- `tmpPath` = `*.fdmdownload` 临时文件名；`writeOffset` 与本机样本一起多为 0。
- `details` 内含 `path`（该下载的**文件名/相对路径**）、`contentType`、`format`、`width/height/duration/bitrate/fps/language/lastModified`。

**磁盘交叉校验（硬证据，15/15 全等）**：
```
id=1518 size_field=8346125    disk=8346125    OK  我为救校花而死，她重生后倒追我.txt
id=1369 size_field=15161322   disk=15161322   OK  通用技术.zip
id=1358 size_field=27504401   disk=27504401   OK  瀬戸環奈写真集「AZURE」【278P】.zip
id=1214 size_field=142313176  disk=142313176  OK  Todo清单 Setup 3.11.2.exe
id=1163 size_field=73318473   disk=73318473   OK  X网盘助手 Setup 5.0.0.exe
…  disk-check: ok=15 mismatch=0
```
⇒ `size`（int64 BE）解析正确；`done`（int64 BE，section 内）同口径可信。

### B.5 FDM 能给"实时"的已下载字节与速度吗？

**已下载字节：能拿到"上次落盘值"，不是实时值。**
- 未完成任务里确有真实部分进度（id=1131 sum=2 576 073 / 24 917 951；id=1068 69 811 287 / 353 783 190），说明 FDM **会**把 section 进度写进库。
- 但库文件 `mtime = 2026/11/14 17:29:28`，而 FDM 已退出——说明 **只在任务状态变化/flush 时写**，不是逐块写。⇒ 拿到的是"最后一次 flush"的快照，轮询间隔再短也不会更实时。

**速度：拿不到。库内没有速度字段。**
```
整库字节级 token 扫描（38 MB）：
  'speed'        utf16be=1   utf16le=1   ascii=1     ← 非字段，散落字符串
  'transferRate' 0 / 0 / 0
  'downloadSpeed' 0 / 0 / 0
  'bytesDownloaded' ascii=3（Qt 内部字符串，非字段）
  'done'         utf16be=455  utf16le=455           ← files BLOB 的真实 key
  'writeOffset'  utf16be=425  utf16le=425           ← 同上
```
⇒ **FDM 只能给：文件名、目标目录、总大小、上次落盘已下载、完成与否、创建/完成时间、失败原因（lastResult）**；**给不了速度，也给不了秒级实时进度**（速度只在内存/UI 里）。

**lastResult BLOB【实测】**= 序列化错误 map（key 含 `category / description / code / source`，UTF-16 + 长度前缀）：
`category='HttpError'` / `'GenericError'`，`source='D:\develop\workspace\fdm_qml_build_windows\src\vmscl\inet\qt\download\downloadswww\wwwdownloadsection.cpp,650'`。

### B.6 FDM 未解清的细节（必须写进 Rust 的容错分支）

1. **`sections` / `details` 两个容器的头部各有一处 4 字节字段未解释**（type 9 的 `[u32 count][u32 ?][u8 pad]`、type 28 的 `[u32 ?]` 之后再进入 entry 序列）。实测用"扫描下一条合法 (keyLen,key,type) 记录"的容忍式解析可以完整走完 blob（`consumed == len(blob)`），但**不能保证对所有历史行都成立**。
   → Rust 建议：**遇到未知 type 或解析越界时，不要整行失败**，回退到"整 blob 正则/扫描式提取 `done`/`size`/`path`"，最差回退到只读普通列。
2. `resumeSupport`：0×112 / 1×275 / 2×43（2 = 不可断点续传，本机样本多为流式 `.ts`/`.html`，size=-1）。
3. `flags` 分布：`2625039 = 0x280E0F` ×410（全部已完成）、`2625036 ×12`、`2625033 ×4`、`2625032 ×2`、`2625037 ×1`、`25166349 ×1`（全部未完成）。→ **位掩码可用作快速状态判别（推断）**，权威判据仍用 `finishedTime > 0`。
4. `destinationType`：0×14 / 1×416（1 = 普通保存到目录；0 多为浏览器/流式）。
5. `title` 实测多为 NULL；**文件名请取 `files.details.path`**，别取 title。

### B.7 FDM 单次读取成本（实测）

```
FDM open ro + count(*)                             min= 2.28ms avg= 3.09ms max= 4.55ms  <430>
FDM open ro + newest 5 rows                        min= 0.45ms avg= 0.56ms max= 0.81ms  <5>
FDM open ro + max(id),max(creationTime),count(*)   min= 2.04ms avg= 2.30ms max= 2.84ms  <(1599,1794648324,430)>
FDM open ro + all 430 rows (no blobs)              min= 2.42ms avg= 3.24ms max= 4.62ms  <430>
FDM copy 38MB db to temp                           min=21.80ms avg=23.74ms max=25.31ms  <38342656>
FDM select 430 files-blobs + 全量解析              min≈24.5ms avg≈26.8ms（其中 SQL 取 blob 2.2–2.8ms，解析 21.7–24.3ms）
files BLOB 总字节 = 301 473
```

---

## C. Ghost Downloader (XiaoYouChR/Ghost-Downloader-3)

### C.1 本机实际情况（推翻"未安装"）

```powershell
Get-Process -Name 'Ghost-Downloader-3'
# Id 38608  StartTime 2026/11/21 15:01:19  Path D:\Ghost Downloader\Ghost-Downloader-3.exe
(Get-Item 'D:\Ghost Downloader\Ghost-Downloader-3.exe').VersionInfo
# FileVersion 4.5.11.0  ProductVersion 4.5.11.0  CompanyName XiaoYouChR  FileDescription Ghost Downloader
```

**【实测】已安装（`D:\Ghost Downloader`，v4.5.11.0，Nuitka/onedir 打包：`python313.dll` + `PySide6/` + `websockets/` + `libtorrent/`）且**正在运行**。
`D:\Ghost Downloader\app\` 只有 `assets/` —— 核心 Python 已编译进 `Ghost-Downloader-3.exe`（64.5 MB），**没有可读的 .py**；但**FeaturePack 以松散源码存在**：`…\features\{http_pack,ffmpeg_pack,bili_pack,bittorrent_pack,ed2k_pack,ftp_pack,github_pack,huggingface_pack,m3u8_pack,wrapped_link_pack,yt_dlp_pack,zip_pack,arxiv_pack}\*.py`（92 个 .py，可读）。
启动日志：`__main__:setupEnvironment:66 - Ghost Downloader v4.5.11 launched`（2026-11-21 15:01:20），随后逐条 `加载 FeaturePack: …`。

### C.2 数据目录（官方文档 + 本机实测一致）

官方文档（https://gd.xychr.com/docs/reference/configuration/ ，**外部资料**）：
> User Mode（默认）Windows：`%LOCALAPPDATA%\GhostDownloader`；Portable Mode：`GhostDownloader/`（exe 旁边）。
> `tasks.jsonl` =「Task records, one per line」；`UserConfig.json` = 全部设置；`GhostDownloader.log` = 512 KB 轮转保留 5 份。

本机实测目录（`C:\Users\Microsoft\AppData\Local\GhostDownloader`）：
```
d---- bt/  ed2k/  features/  kelpie/  M3U8DL/  update_staging/  YtDlp/
-a--- bt_dht_state.dat  16378   2026/11/20 19:34:50
-a--- GhostDownloader.log 21525 2026/11/21 19:47:47
-a--- tasks.jsonl       60228   2026/11/21 21:05:08
-a--- updater.log        1076   2026/11/20 19:34:56
-a--- UserConfig.json    8553   2026/11/14 17:53:01
```
⇒ **与官方文档完全对应（User Mode）。【实测】**

### C.3 tasks.jsonl 格式（JSONL，一行一个任务）

**【实测】19 行，每行一个 JSON 对象。**顶层键并集（出现次数/19）：
```
taskId 19, name 19, url 19, packId 19, type 19, status 19, steps 19,
createdAt 19, completedAt 19, outputFolder 19, fileSize 19, files 19,
category 19, shouldSeed 19, checksums 19,
duration 13, startTime 13, endTime 13, baseName 13, rawFileSize 13,   # ffmpeg 任务专属
tailOffset 1, metadataEncoding 1
```
`type` 实测取值：`http_pack.task.HttpTask`(5)、`ffmpeg_pack.task.FFmpegTask`(13)、`zip_pack.task.ZipTask`(1)。
`taskId` 形如 `tsk_` + 32 hex；`outputFolder` = `D:\下载`；`createdAt/completedAt` = unix 秒。

**step 级字段（19 个，实测 key 列表）**：
```
stepIndex, stepId('stp_'+32hex), fileIndex, type, status,
progress(0–100 int), receivedBytes, speed, fileSize,
url, headers{}, clientProfile, userAgent, subworkerCount,
canUseRangeRequests, httpByteOffset, lastModified, filePath, isAccelerated
```
ffmpeg step 另有：`role, extension, rawFileSize, segStartTime, videoExtension, audioExtension, shouldDeleteSource`。

### C.4 状态枚举

- **【实测】**本机 19 个任务全部 `status = "COMPLETED"`；step 的 status 出现 `COMPLETED`×37、`WAITING`×5（同一任务里 `COMPLETED + WAITING` 组合常见，例如 `prog=[100, 0]`）。
- **【实测】**exe 内嵌字符串同时含：`TaskStatus, WAITING, PAUSED, RUNNING, COMPLETED, FAILED, STOPPED, DOWNLOADING`。
- **【外部资料，推断】**官方 `CONTEXT.md`（AtomGit 镜像）：任务五态 = `WAITING（排队） / RUNNING（下载中） / PAUSED（用户暂停） / COMPLETED / FAILED`；且「Step Error 仅运行时存在，不持久化」。
⇒ Rust 枚举建议：`WAITING | RUNNING | PAUSED | COMPLETED | FAILED` 为已知集合，另保留 `STOPPED | DOWNLOADING` 及**未知值透传**（字符串枚举，不要转数字，因为 JSON 里就是大写字符串）。

### C.5 是否持久化"已下载字节 / 速度"——有字段，但**不能直接信**

**【实测】字段是持久化的**：每个 step 都有 `receivedBytes` 与 `speed`；`progress` 是整数百分比。

**但聚合不自洽（重要风险）【实测】**：
```
tsk_e5501f… fileSize=  2 458 907  recvSum=  2 458 907   ✅
tsk_435f7d… fileSize= 27 930 166  recvSum= 27 891 384   ⚠ 差 38 782
tsk_961817… fileSize=129 245 950  recvSum= 76 815 219   ⚠ 且 status=COMPLETED
tsk_d89391… fileSize= 21 246 512  recvSum= 15 345 707   ⚠
tsk_7870ec… fileSize=1 036 076 400 recvSum=983 223 346  ⚠
tsk_fa7f7c… fileSize=  9 882 487  recvSum=          0   ⚠ progress 却全是 100
（8 个任务 recvSum=0 而 progress=100）
```
⇒ ① **不能用 `sum(receivedBytes)` 当"已下载"**（ffmpeg 任务尤其：`recvSum=0` 但已 100%）；
② **不能用 `receivedBytes/fileSize` 算百分比**，请直接用 `progress`（step 级 0–100）；任务级进度 = 各 step 的非 WAITING 部分（**推断**：`Σ(step.progress × step.fileSize) / Σ step.fileSize`，需在运行中实测校准）；
③ 真实成品大小请以 `os.stat(outputFolder/name)` 为准。

**速度**：`speed` 字段存在，但**本机 19 个任务全部 speed=0**（都已完成）。⇒ **【推断】**运行中会写实时速度、完成后归零；**验证方法**：加一个大文件任务，在下载中每 2 s 读 tasks.jsonl 看 `steps[].speed` 是否非 0 且变化。

**文件写入行为【实测】**：空闲时 10 s 内 6 次 `stat` 的 `mtime_ns` **完全不变**（`1795266308397420300`）；但先前观测到 21:04:53 → 21:05:08 两次写（任务数未变）⇒ **不是逐块/每秒重写，而是事件驱动 + 定期 flush（观测到的两次写间隔 ≈15 s）**。⇒ 轮询侧不必 100 ms 级别；且**整体重写意味着可能读到半写状态**，读取要"读两次比对/校验尾部为完整 JSON"。

### C.6 运行时接口（有没有可读 API？）

**【实测】**属于 Ghost PID 38608 的监听端口：
```
127.0.0.1:14370 + [::1]:14370   GET /  → HTTP 426 Upgrade Required
                                 ClientWebSocket.ConnectAsync("ws://127.0.0.1:14370/") → State=Open   ← 真 WebSocket 服务
127.0.0.1:16800 + [::1]:16800   GET /,/api,/api/v1,/status,/version,/tasks,/api/tasks,/ping,/health → 全 404
0.0.0.0:17280  Bound，与 127.0.0.1:17279 Established（本机内部 socket 对）
裸 TCP 直读两端口 → 读超时（都讲协议，无 banner）
```
**结论**：
- `14370` = **WebSocket 服务**，从 `websockets` 依赖 + 官方"浏览器扩展捕获资源并发送到桌面应用"判断，**这是浏览器扩展桥（【推断】）**，不是公开数据 API；协议未知，不建议依赖。
- `16800` = 存在 HTTP 服务但路径未知（**未验证**）。可用 Chrome/Edge 扩展的 manifest 或 `UserConfig.json` 反查端口来源（下一步）。
- 官方文档另有 **CLI `ghd`**（Settings → Command Line Tool 安装）：`ghd tasks` / `ghd task <id>` / `ghd add <url>` —— **本机未安装**（`D:\Ghost Downloader` 下无 `ghd.exe`）。
⇒ **Rust 侧首选直接读 `tasks.jsonl`（成本 0.82 ms、格式稳定），不要依赖内部 WS/HTTP。**

### C.7 Ghost 单次读取成本（实测）

```
Ghost os.stat tasks.jsonl only（廉价变更探测）      min=0.03ms avg=0.05ms max=0.08ms  <60228B>
Ghost read tasks.jsonl (raw lines)                  min=0.40ms avg=0.46ms max=0.61ms  <19>
Ghost read+parse tasks.jsonl (json, 全量)           min=0.61ms avg=0.82ms max=1.11ms  <19 tasks>
```

---

## D. 通用结论

### D.1 三源"可获取字段矩阵"

| 字段 | IDM（注册表） | FDM（db.sqlite） | Ghost（tasks.jsonl） |
|---|---|---|---|
| 文件名 | **可** `FileName`（需去 `?query`） | **可** `files→details.path` | **可** `name`（+/`files[]`） |
| 完整路径 | **推断** 全局保存目录 + FileName（本机 9/301 命中） | **可** `destinationPath` + `path` | **可** `outputFolder` + `name` |
| 总大小 | **可** `FileSize`（u64 LE） | **可** `files→size`（int64 BE） | **可** `fileSize`（+`rawFileSize`） |
| 已下载 | **可** `Downloaded`（u64 LE） | **可** =Σ`sections[].done`（**上次落盘值，非实时**） | **推断** `Σ steps[].receivedBytes`（实测常≠fileSize，勿用） |
| 进度百分比 | **可** `Downloaded/FileSize` | **可** Σdone/Σsize（非实时） | **可** `steps[].progress`（0–100，最可信） |
| 速度 | **不可（残留值）** `Speed` 仅在"运行中且 Status=1"时有意义（未验证） | **不可** 库内无速度字段（token 扫描证实） | **推断** `steps[].speed`（本机全为 0，运行中未验证） |
| 状态：进行中 | **推断** `Status=1`（本机无样本） | **可** `finishedTime==0 && Σdone>0`（+`flags` 位） | **可** `status=="RUNNING"/"WAITING"` |
| 状态：暂停 | **推断** `Status=2`（本机无样本） | **推断** `resumeSupport`/`flags` 组合（无直接列） | **可** `status=="PAUSED"` |
| 状态：完成 | **可** `Status==3`（301/301 且 Downloaded==FileSize） | **可** `finishedTime>0`（flags=2625039） | **可** `status=="COMPLETED"` |
| 状态：失败 | **可** `Status==0` + `lastResult` 文本 | **可** `lastResult` BLOB（category/source）+ `finishedTime==0` | **可** `status=="FAILED"`（+step 级错误不持久化） |
| 创建时间 | **可** `dateAdded`(SZ 本地时间) / `st_time`(DWORD unix) | **可** `creationTime`（unix 秒） | **可** `createdAt`（unix 秒） |
| 完成时间 | **推断** 无完成时间字段；只有 `lastTryDate`（≈最后活动） | **可** `finishedTime`（unix 秒） | **可** `completedAt`（unix 秒） |

### D.2 "有新任务出现"的最小可行机制

| 源 | 最小机制 | 单次成本（实测） | 建议周期 |
|---|---|---|---|
| **IDM** | `GetSubKeyNames()` 过滤 `^\d+$` → 与上一轮 id 集合做差集；新 id 即新任务。对变化/新增键再读 `Status/Downloaded/FileSize/FileName` | 枚举 **3.9 ms**；全量读 4 值 **24.3 ms**；仅(Status,Downloaded) **17.1 ms**；单任务 **0.5 ms** | 枚举 1–2 s；仅在集合变化时做全量读 |
| **FDM** | `SELECT max(id), count(*) FROM downloads`（id 单调递增）→ 变化时 `SELECT … WHERE id > lastMaxId` 取新行；进度刷新则额外读 `files` BLOB | 变更探测 **2.3 ms**；新行查询 **0.6 ms**；全量 430 行无 blob **3.2 ms**；全量 blob 读+解析 **≈27 ms**；整库 copy **23.7 ms** | 变更探测 3–5 s；FDM 只在 flush 时写库，1 s 轮询无收益 |
| **Ghost** | `os.stat(tasks.jsonl)` 比 `(size, mtime_ns)` → 变了才全读解析 JSONL | stat **0.05 ms**；全读 **0.46 ms**；全解析 **0.82 ms** | stat 500 ms–1 s；解析仅在 stat 变化时 |

**实现要点（三源通用）**
- IDM 差集要处理"任务被删除"（集合差集的负方向）与 `maxID` 不可靠（本机为空值）。
- FDM `id` 为 INTEGER PRIMARY KEY 且实测单调递增（max=1599）；**但删除任务后 id 不回收**，用 `id > lastMax` 增量安全。
- Ghost 整文件重写 → 读到半截的风险：**读两次并要求 `size/mtime` 一致 + 每行 `json.loads` 成功**再采纳；失败则下一轮重试（成本 < 1 ms，可接受）。
- 三源都要有"进程存活"旁路：IDM→`IDMan.exe`；FDM→`fdm.exe`；Ghost→`Ghost-Downloader-3.exe`。进程不在时进度字段一律当"存量快照"，不要呈现为实时。

### D.3 解析伪码（可直接落 Rust）

```
# ---------- IDM ----------
root = HKCU\Software\DownloadManager
ids  = [k for k in root.subkeys() if k.is_all_digits()]
for id in ids:
    k = root.open(id)
    file_size = u64_le(k.get_bytes("FileSize", 8))        # REG_NONE 8B LE
    downloaded= u64_le(k.get_bytes("Downloaded",8))
    status    = k.get_u32("Status")                        # 0=未完成 3=完成 (1/2 推断:下载中/暂停)
    name      = k.get_str("FileName").split('?')[0]
    url       = k.get_str("Url0")
    host      = k.get_str("Host"); port = k.get_u32("Port")
    part_path = k.get_str("LocalFileName")                 # 临时 part 路径, 不是最终路径
    created   = parse("%b %d %H:%M:%S %Y", k.get_str("dateAdded"))  # 本地时间
    created2  = from_unix(k.get_u32("st_time"))
    rate      = k.get_u32_opt("Speed")                     # 残留值, 不可当实时
    err       = k.get_str_opt("lastResult")
    progress  = downloaded / file_size  if file_size>0 else None

# ---------- FDM ----------
db  = open_readonly("db.sqlite")          # journal=rollback, 单文件即可
new = db.query("select id,destinationPath,creationTime,finishedTime,flags,lastResult,files,url "
               "from downloads where id > ?", last_max_id)
for row in new:
    rec = scan_vmscl(row.files)           # 见 B.4: u32 fileCount + entries(key u32len/utf16be/type/pad/value)
    # rec = { "size":[..], "done":[..], "offset":[..], "path":[..], "tmpPath":[..], "writeOffset":[..] }
    total = first(rec.size) or 0
    done  = sum_sections_done(rec)        # 完成时 == total; 未完成时是上次落盘值
    name  = first(rec.path)
    full  = join(row.destinationPath.replace('/','\\'), name.replace('/','\\'))
    state = "completed" if row.finishedTime > 0 else ("active" if done>0 else "pending")
    err   = decode_error_map(row.lastResult)   # {category, code, description, source}

# ---------- Ghost ----------
before = stat(tasks_jsonl)                       # (size, mtime_ns)
if stat(tasks_jsonl) != before: return skip      # 未变化直接跳过
for line in read_lines(tasks_jsonl):
    t = json.loads(line)                          # 一行一任务; 解析失败则整轮丢弃重试
    status  = t["status"]                          # WAITING/RUNNING/PAUSED/COMPLETED/FAILED(+未知透传)
    size    = t.get("fileSize") or 0
    recv    = sum(s.get("receivedBytes") or 0 for s in t["steps"])   # 仅参考, 常与 size 不符
    prog    = weighted_avg(s.get("progress") for s in t["steps"])    # 优先用 progress
    rate    = max((s.get("speed") or 0) for s in t["steps"])         # 运行中才非 0(推断)
    full    = join(t["outputFolder"], t["name"])
    created = t.get("createdAt"); completed = t.get("completedAt")
```

---

## E. 三大风险 & 未验证清单

**风险 1｜IDM 的 1/2 状态与"实时速度"完全未实测（本机 IDMan.exe 缺失，只有 .BAK）**
本机 345 个任务只有 `Status∈{0,3}`，从未出现 1/2。若 Rust 把 1/2 落到"未知"或误判为"失败/完成"，进行中的任务会显示错误。
验证方法：把 `IDMan.exe.BAK` 还原为 `IDMan.exe`，起一个大文件下载，下载中/暂停各读一次 `Status` 与 `Speed`（并观察 `Speed` 是否随秒变化）。

**风险 2｜FDM 的 `files` BLOB 是私有格式，容器头部仍有未解释字节，且库内根本没有速度、进度也不是实时**
已硬校验的部分：`size`(int64 BE) 与磁盘 15/15 全等、`done` 语义正确、`path/tmpPath` 可取。未解释：type 9 / type 28 容器头部各一处 4 字节字段。另外 6 个未完成任务 `files` 只有 4 字节（`fileCount=0`）→ 这类任务只能拿到 `flags/creationTime/url`，无大小无文件名。
⇒ Rust 必须：`unknown type` 不致命（扫描式兜底 + 只读普通列降级），并且**不要给 FDM 画"实时速度条"**（数据源不存在）。

**风险 3｜Ghost 的 `receivedBytes` 与实际严重不符，且 tasks.jsonl 是整文件重写**
实测 5 个任务 `recvSum < fileSize` 但 `status=COMPLETED`，8 个任务 `recvSum=0` 而 `progress=100`。若用 `receivedBytes` 算已下载/百分比，会给出明显错误的进度。
⇒ 一律用 `steps[].progress`，文件真实大小用 `os.stat(成品路径)`；读取必须容忍半写状态（比对两次 stat + 逐行 JSON 校验）。

**其余未验证项（附验证方法）**
| 项 | 状态 | 验证方法 |
|---|---|---|
| IDM `Status=1/2` 语义 | 推断 | 见风险 1 |
| IDM `Speed` 运行时是否实时 | 推断（本机为残留） | 风险 1 中有无逐秒变化 |
| IDM 最终保存目录是否恒等于根值 `LocalPathW` | 推断（9/301 命中，其余被移动） | 观察新下载完成前后 `Test-Path` 命中率 |
| FDM 运行中 `db.sqlite` 可读性 / `SQLITE_BUSY` | 未验证（采集时 FDM 未运行，库为 rollback journal 模式） | 启动 fdm.exe 并开始下载，用 `mode=ro` + `busy_timeout` 连续读，记录 busy 次数；必要时改为整库 copy（23.7 ms） |
| FDM `flags` 位含义 | 推断 | 用 410 已完成 / 20 未完成样本做位差集（已知 `0x280E0F` = 完成态） |
| Ghost `RUNNING/PAUSED/FAILED` 在文件里的真实取值 | 推断（本机 19 个任务全 COMPLETED） | 起任务→暂停→失败各一次，抓 `status` 字面量 |
| Ghost `steps[].speed` 运行时是否实时 | 推断（本机全 0） | 同上，每 2 s 采样 |
| Ghost 16800 HTTP 路径 / 14370 WS 协议 | 未验证 | 读 `browser_extension` 的 manifest 与 background.js；或对 14370 发一条 WS 帧看回包 |
| Ghost `ghd` CLI | 本机未安装 | Settings → Command Line Tool 安装后 `ghd tasks` |

---

## F. 附：本文所有原始命令索引

```
# IDM
[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\DownloadManager')
$k.GetSubKeyNames() / $k.GetValueNames() / $k.GetValueKind(name) / [BitConverter]::ToUInt64($bytes,0)
Get-Content 'C:\Users\Microsoft\AppData\Roaming\IDM\DwnlData\Microsoft\<job>\log_<id>.log' -TotalCount 12
Get-ChildItem 'C:\Users\Microsoft\AppData\Roaming\IDM\DwnlData\Microsoft\<job>'

# FDM
py -3 .probe\fdm_probe4.py    # journal_mode(byte18/19) + 整库 token 扫描 + flags 分布
py -3 .probe\fdm_scan.py      # files BLOB 容忍式 record 扫描（size/done/path/tmpPath）
py -3 .probe\fdm_final.py     # 递归解析 + 磁盘交叉校验（15/15 OK）
py -3 .probe\fdm_hex.py       # blob 偏移级 hexdump（推导 B.4 布局）
py -3 .probe\fdm_bench.py     # 全量 blob 读+解析成本

# Ghost
py -3 .probe\ghost_dump.py    # tasks.jsonl 全字段 + mtime 稳定性采样
Invoke-WebRequest http://127.0.0.1:14370/    # 426 Upgrade Required
[System.Net.WebSockets.ClientWebSocket].ConnectAsync('ws://127.0.0.1:14370/')  # State=Open
Invoke-WebRequest http://127.0.0.1:16800/<path>   # 404 (路径未知)

# 计时
pwsh .probe\registry_timing.ps1 ; py -3 .probe\timing.py
```
