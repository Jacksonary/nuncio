# Nuncio 方案（v0.1）

> 状态：方案评审中 · 更新：2026-11-12

## 0. 一句话

Nuncio 是一个轻量桌面 PIM 客户端（**邮件 + 日历 + 联系人 + 待办**），交互参考经典桌面邮件与日历应用，功能参照 [Wino Mail](https://github.com/bkaankose/Wino-Mail)，去掉 AI、账户上限、云账号与一切自有服务端。

---

## 1. 背景与参照

### 1.1 Wino Mail 调研要点

| 项 | 情况 |
|---|---|
| 技术栈 | .NET 10 / C#；Windows 端 WinUI 3 + MSIX + Native AOT；邮件阅读/编辑用 WebView2 |
| 规模 | 手写 C# 约 45 万行（另有约 30 万行自动生成的 Graph 模型），40+ csproj，4 个平台 |
| 邮件同步 | IMAP 走 MailKit，QRESYNC → CONDSTORE → UID 三级降级；Gmail 用 History API，Outlook 用 Graph delta |
| 存储 | SQLite 存元数据，MIME 原文以 `.eml` 落盘 |
| 账户限制 | `Constants.FreeAccountLimit = 3`，超出需买 Unlimited Accounts |
| AI | `Wino.Core.ML`（ONNX + Magika）、`Wino.Agent.Runtime`、MailIntelligence / Briefing 等，约 11.5 万行 |
| 许可证 | GPL-3.0 |

**借鉴的设计思路**：元数据优先同步、多级 IMAP 同步策略、乐观 UI + 离线操作队列、合并收件箱、MIME 原文落盘、服务商能力标记（`AccountProviderFeature`）。

### 1.2 Wino 服务端及取舍

Wino 有三个域名：`api.winomail.app`（Wino-Mail-API）、`relay.winomail.app`（Wino-Mail-Relay）、`www.winomail.app`。两个后端仓库在 GitHub 上都是 404，推测是私有的。

| Wino 服务端功能 | Nuncio 处理 |
|---|---|
| Wino 账号（注册/登录/资料） | 不做 |
| 收费：无限账户兑换、商店购买校验、AI 订阅 | 不做 |
| AI：改写/摘要/翻译/助手/Agent 记忆 | 不做 |
| 设置云同步（加密 snapshot） | 改为本地加密导出/导入文件 |
| Push Relay：Gmail Pub/Sub、Graph webhook 中转；移动端代管 IMAP IDLE | 不需要：邮件统一走 IMAP IDLE，客户端直连；日历/联系人/待办用定时轮询 |

邮件内容本来就不经过 Wino 服务端。去掉这些服务后，Nuncio 运行时**只和邮件、日历服务商通信**。

### 1.3 许可证

只借鉴架构思路，用 Rust 从零实现，不移植、不翻译 Wino 代码，所以许可证不受 GPL 约束。本项目采用 **Apache License 2.0**。UI 风格参考经典桌面邮件客户端，但图标和品牌都用自己的，不使用微软的素材（Fluent System Icons 是 MIT 许可，可以用）。

---

## 2. 目标与范围

### 2.1 目标

| 维度 | 指标 |
|---|---|
| 轻 | Windows 安装包 ≤ 12 MB；托盘常驻（窗口关闭）≤ 40 MB |
| 快 | 冷启动到首屏 < 1 s；万封邮件列表滚动不掉帧 |
| 零服务端 | 数据只在本机与服务商之间流动；无遥测、无统计上报 |
| 无限制 | 账户数不限、无内购、无登录墙 |

### 2.2 平台与产物

| 平台 | 产物 | 优先级 |
|---|---|---|
| Windows 10/11 x64 | `Nuncio_x.y.z_x64-setup.exe`（NSIS） | 主力 |
| macOS（arm64 / x64） | `.dmg` | 顺带 |
| Linux x64 | `.deb`、`.AppImage` | 顺带 |

### 2.3 功能范围

**邮件**
- 账户：Outlook.com/M365、Gmail、iCloud、Yahoo、QQ/163/企业邮、通用 IMAP/SMTP、POP3（后期）
- 合并收件箱（Linked inbox）、账户颜色、多身份/别名（send-as）、每身份签名
- 文件夹：树形结构、收藏置顶，新建/重命名/删除/移动；按 SPECIAL-USE 识别角色
- 列表：虚拟滚动、会话视图开关、筛选（全部/未读/已标记/有附件）、密度与预览行数、悬停快捷操作（可自定义）
- 操作：已读/未读、旗标、删除、归档、移动、标记垃圾、多选批量；全部离线可用
- 阅读：安全渲染、远程图片控制（按发件人信任）、阅读视图（readability）、暗色下"邮件白底"开关、新窗口打开
- 撰写：富文本（格式/插入/选项三个分区）、附件、内嵌图片、表格、回复/全部回复/转发、草稿同步、发件箱离线排队、收件人补全、优先级
- 搜索：本地 FTS5（支持中文）+ 服务端 `SEARCH` 回退
- 本地邮件规则（条件 → 移动/已读/旗标/删除）
- 打印、另存为/打开 `.eml`、mailto 处理

**日历**
- 日 / 工作周 / 周 / 月 / 年 / 日程视图，迷你月历，日历列表显隐与颜色
- 事件增删改、全天事件、重复事件（RRULE，含例外）、时区
- 提醒（本地调度，窗口关闭也能弹）
- 会议邀请：解析邮件中的 ICS，接受/暂定/拒绝，发送 iTIP 回复
- 打开/导入 `.ics`，订阅 `webcal://`

**联系人**
- 列表、搜索、分组/标签、收藏
- 详情编辑：姓名、多邮箱、电话、地址、生日、备注、头像
- 从联系人直接写信；撰写时自动补全（联系人 + 往来地址）
- 导入/导出 `.vcf`

**待办**
- 智能列表：我的一天、重要、已计划、全部、已完成
- 自定义列表（颜色）、子步骤、截止日期、提醒、重复、备注
- 按服务商能力降级（例如 Google Tasks 没有"重要"和提醒时间）

**通用**
- 底部模块切换栏（邮件/日历/联系人/待办/设置），与老版 Mail 一致
- 浅色/深色/跟随系统，强调色；Win11 下 Mica 背景
- 托盘常驻、开机自启、Toast 通知、任务栏未读角标
- 本地加密备份/恢复（设置 + 账户配置，可选包含凭据）
- 界面语言：zh-CN、en-US

### 2.4 明确不做

AI 及一切智能功能、账户上限与内购、Wino 式云账号、自有服务端与推送中转、遥测/崩溃上报、Widget、主题编辑器、Exchange ActiveSync/EWS。

### 2.5 服务商支持矩阵

| 服务商 | 邮件 | 日历 | 联系人 | 待办 |
|---|---|---|---|---|
| Gmail | IMAP/SMTP + XOAUTH2（X-GM-EXT-1） | Google Calendar API v3 | Google People API | Google Tasks API |
| Outlook.com / M365 | IMAP/SMTP + XOAUTH2 | Microsoft Graph | Microsoft Graph | Graph To Do |
| iCloud | IMAP/SMTP（应用专用密码） | CalDAV | CardDAV | CalDAV VTODO（新版"提醒事项"可能无法通过 CalDAV 访问，待验证） |
| QQ / 163 / 企业邮等 | IMAP/SMTP（授权码） | CalDAV（视服务商支持） | CardDAV（视服务商支持） | — |
| 通用（Fastmail、Nextcloud…） | IMAP/SMTP、POP3 | CalDAV | CardDAV | CalDAV VTODO |
| 本地 | — | 本地日历 | 本地通讯录 | 本地列表 |

邮件统一走 IMAP：能用 IDLE 实时推送，不需要服务端；同步引擎也只有一套。日历、联系人、待办没有统一协议，按服务商分别写适配器。

---

## 3. 技术选型

**Tauri 2 + React 18 + Rust** 技术栈。Windows 10/11 自带 WebView2，不用打进安装包。

不选的方案：
- WinUI 3：只能在 Windows 上构建，在 Mac 上没法开发
- egui / Slint：无法渲染 HTML 邮件

| 层 | 选型 | 备注 |
|---|---|---|
| IMAP | `async-imap` | IDLE 成熟；CONDSTORE 只部分支持，QRESYNC 不支持，必要时手写原始命令。备选 `imap-next` / `imap-codec` |
| SMTP | `lettre` | 支持 XOAUTH2、STARTTLS |
| POP3 | 自写（协议很简单） | 后期 |
| MIME | `mail-parser` / `mail-builder` | 处理 GBK、GB18030、Big5、ISO-2022-JP 等编码 |
| HTML 清洗 | `ammonia` | 白名单 |
| HTTP | `reqwest`（rustls） | Google / Graph / DAV |
| DAV | `reqwest` + `quick-xml` 自写 PROPFIND/REPORT | sync-collection（RFC 6578），不支持时退回 ctag/etag |
| iCal / vCard | `calcard`（Stalwart）或 `icalendar` + 自写 vCard 解析 | 待评估 |
| 重复事件 | `rrule` + `chrono-tz` | |
| 存储 | `rusqlite`（bundled，WAL，FTS5） | |
| 凭据 | `keyring` | Linux 没有 Secret Service 时退回 AES-256-GCM 加密文件 |
| 前端 | React 18 + Vite + TypeScript | |
| 列表 | `@tanstack/react-virtual` | |
| 状态管理 | Zustand（UI 状态）+ TanStack Query（IPC 数据缓存） | |
| 编辑器 | Tiptap（ProseMirror） | |
| 样式 | 手写 Fluent 2 设计 token 的 CSS + `@fluentui/react-icons` 按需引入 | 不引入整套组件库 |
| 日期 | 原生 `Intl` + `date-fns` | 时区计算放在 Rust 端 |
| i18n | 轻量字典 / i18next | |
| IPC 类型 | `tauri-specta` 自动生成 TS 类型 | 不好用就手写 |
| Tauri 插件 | notification、single-instance、autostart、updater、opener、dialog、deep-link | |

---

## 4. 架构

```
┌──────────────────────── WebView (React) ─────────────────────────┐
│ Shell │ Mail(列表/阅读 sandbox iframe/撰写) │ Calendar │ People │ ToDo │ Settings │
└───────────────▲──────── Tauri IPC：command(请求) + event(推送) ──┘
                │
┌───────────────┴──────────────── Rust Core ───────────────────────┐
│ app       ：commands、窗口/托盘、通知、单实例、mailto/文件关联       │
│ auth      ：OAuth2 PKCE 回环、token 刷新、keyring、服务器自动发现    │
│ mail      ：每账户一个 SyncActor（IDLE + worker 两条连接）、OpQueue、 │
│             SMTP/发件箱、MIME 解析/清洗/构建、规则引擎               │
│ pim       ：Google / Graph / CalDAV / CardDAV 适配器（Provider trait）│
│ scheduler ：定时同步、提醒队列、睡眠唤醒/断网重连                      │
│ store     ：SQLite（WAL、FTS5、迁移）+ mime/ 附件缓存（LRU）          │
└──────────────────────────────────────────────────────────────────┘
        │ IMAP/SMTP(TLS)        │ HTTPS(Google/Graph/DAV)
        ▼                       ▼
     邮件服务商              日历/联系人/待办服务商
```

### 4.1 进程与生命周期
- 单进程 Rust 加 WebView。**关闭窗口时销毁 WebView**，只保留 Rust 后台和托盘；点托盘再重建窗口。
- 单实例：再次启动（包括 mailto 和文件关联打开）时，把参数转发给已在运行的实例。

### 4.2 IPC 约定
- 列表类数据按页拉取（`mail_list(scope, cursor, limit, filter)`），不一次推整个列表。
- Rust 端变更后发事件，例如 `mail://changed {folderId, ids}`、`calendar://changed {calendarId, range}`、`sync://status`。前端收到后让对应的 Query 失效重新拉取。
- 邮件正文由 Rust 清洗后返回，前端只负责把它放进 sandbox iframe。

### 4.3 Provider 抽象（pim）
```rust
trait CalendarProvider { list_calendars; sync_events(cursor) -> Delta; upsert_event; delete_event; respond_invite; }
trait ContactProvider  { list_books; sync_contacts(cursor) -> Delta; upsert; delete; }
trait TaskProvider     { list_lists; sync_tasks(cursor) -> Delta; upsert; delete; capabilities(); }
```
每个账户在 `provider_capability` 表里记录支持哪些功能，UI 据此显示或隐藏相应功能。

---

## 5. 数据与存储

### 5.1 目录（Tauri `app_local_data_dir`）
```
nuncio.db                 SQLite（WAL）
mime/<account>/<xx>/<hash>.eml   正文缓存（LRU，默认上限 500 MB）
attachments/              按需下载的附件
logs/                     本地滚动日志（不上传）
```

### 5.2 主要表
| 模块 | 表 |
|---|---|
| 通用 | `account`、`identity`（别名/send-as）、`signature`、`provider_capability`、`sync_state`、`settings`、`pending_op` |
| 邮件 | `folder`（uidvalidity/uidnext/highest_modseq/role/favorite）、`message`（uid/gm_msgid/gm_thrid/message_id/thread_id/flags/preview/body_cached）、`attachment`、`mail_rule`、`trusted_sender`、`fts_mail`（FTS5） |
| 日历 | `calendar`（sync_token/ctag/color）、`event`（ical_uid/etag/start/end/tz/rrule/exdates/recurrence_id）、`event_attendee`、`event_reminder` |
| 联系人 | `address_book`、`contact`、`contact_email` / `contact_phone` / `contact_address`、`contact_group`、`recipient_history` |
| 待办 | `task_list`、`task`（importance/due/reminder_at/recurrence/my_day_date/parent_id） |

- 重复事件**不预先展开存库**：按视图查询的时间范围在 Rust 端实时展开；提醒调度器只展开未来 48 小时。
- 中文搜索：入库前把 CJK 文本切成 bigram，存进 FTS5（unicode61 分词器）的辅助列，查询时用同样的方式切分。
- 数据库默认不加密，依赖系统磁盘加密（BitLocker / FileVault）。加密方案（SQLCipher）会让体积增加约 1 MB，放在后期视需要再定。

---

## 6. 同步设计

### 6.1 邮件（IMAP）

**连接**：每个账户最多 2 条连接，一条对 INBOX 做 IDLE，一条 worker 负责拉取、执行操作和同步其他文件夹（Gmail 单账户连接上限是 15）。

**首次同步**
1. 先 `LIST`，用 SPECIAL-USE（或 XLIST）识别各文件夹的角色。Gmail 的 `[Gmail]/All Mail` 默认隐藏，标签按文件夹展示。
2. 先同步 INBOX 和收藏的文件夹，下载范围默认"过去 3 个月"（可选 3 天 / 7 天 / 2 周 / 1 个月 / 3 个月 / 全部，和老版 Mail 一致）。
3. 只取元数据：`FLAGS UID INTERNALDATE ENVELOPE BODYSTRUCTURE RFC822.SIZE`，再加 `BODY.PEEK[HEADER.FIELDS (REFERENCES)]`；Gmail 额外取 `X-GM-MSGID X-GM-THRID X-GM-LABELS`。从新到旧，每批 200 封，边拉边通知 UI 刷新。
4. 正文在打开时才下载（`BODY.PEEK[]`），解析后缓存。后台预取最新 50 封未读邮件的正文（可配置）。预览文本从预取的正文里截取；没有正文时，单独拉取 text/plain 部分的前 2 KB。

**增量同步**
- 服务器支持 CONDSTORE：`UID FETCH 1:* (FLAGS) (CHANGEDSINCE <modseq>)` 拿标志位变化，`UID FETCH <uidnext>:*` 拿新邮件。
- 不支持 CONDSTORE：拉取同步范围内的 UID 和 FLAGS，与本地对比。
- 删除检测：每次增量时对比 `UID SEARCH` 的结果，每 12 小时做一次全量 UID 对账。QRESYNC 留到后期优化。
- `UIDVALIDITY` 变化：清空该文件夹的本地缓存，重新同步。

**推送**：每个账户对 INBOX 做 IDLE。收到 `EXISTS` / `EXPUNGE` / `FETCH` 时触发增量同步。每 ≤25 分钟重发一次 IDLE（RFC 2177 要求 < 29 分钟）。其他文件夹每 15 分钟轮询一次（可配置），打开时立即同步。

**离线操作队列（OpQueue）**
1. 用户操作先改本地 SQLite，UI 立即更新。
2. 同时写入 `pending_op`，worker 按顺序执行：`UID STORE`、`UID MOVE`（服务器不支持 MOVE 时用 COPY + `\Deleted` + `UID EXPUNGE`）、`APPEND` 等。
3. 失败按指数退避重试；连续失败则回滚本地状态并提示用户。冲突时以服务端为准。
4. 同类操作合并批量执行（例如多封邮件一起标已读）。

**发送**：用 `mail-builder` 生成 MIME，经 SMTP（XOAUTH2 或 PLAIN）发送。Gmail 会自动把已发送邮件存到"已发送"，不能再 APPEND，否则会重复；Outlook 是否会自动保存待实测；其他服务器需要 APPEND 到 Sent。断网时邮件进发件箱，联网后自动补发。

**会话**：Gmail 直接用 `X-GM-THRID`；其他服务器按 Message-ID / In-Reply-To / References 串联，再用归一化后的主题兜底。

**规则**：新邮件入库后执行本地规则，产生的动作同样走 OpQueue。

### 6.2 日历 / 联系人 / 待办

| 服务商 | 增量机制 |
|---|---|
| Google Calendar | `events.list` + `syncToken`；返回 410 时全量重同步 |
| Google People | `connections.list` + `syncToken`（会过期，过期后全量） |
| Google Tasks | 没有 syncToken：用 `updatedMin` + `showDeleted` + `showHidden` |
| Graph 日历 | `calendarView/delta`（按时间窗口）+ deltaLink |
| Graph 联系人 | `contacts/delta` |
| Graph To Do | `todo/lists/{id}/tasks/delta` |
| CalDAV / CardDAV | `sync-collection`（RFC 6578）；不支持时对比 ctag，再对比各条目的 etag |

- 默认同步频率：日历 15 分钟、联系人 60 分钟、待办 30 分钟，均可配置。打开对应模块时立即同步。
- 写操作同样先改本地，再经 `pending_op` 推到服务端。CalDAV/CardDAV 用 `If-Match: etag` 防止覆盖别人的修改，冲突时以服务端为准并提示。
- Microsoft 账户需要两种 access token（Outlook 资源和 Graph 资源），用同一个 refresh token 分别换取，见附录 B.4。

### 6.3 调度器
- 统一管理所有定时任务、各账户 IDLE 的健康状态、提醒队列。
- 每 30 秒比较一次单调时钟和墙上时钟，跳变说明刚从睡眠唤醒。唤醒或网络恢复后立即补同步，并重建 IDLE 连接。
- 提醒：维护未来 48 小时内的提醒，事件或任务变更时重新计算，到点通过系统通知弹出。

---

## 7. 认证

| 类型 | 流程 |
|---|---|
| Google / Microsoft | OAuth2 授权码 + PKCE：用系统浏览器打开授权页，本地临时监听 `127.0.0.1:<随机端口>` 接收回调。token 存 keyring，过期前自动刷新 |
| 密码 / 授权码 / 应用专用密码 | 存 keyring；IMAP/SMTP 用 PLAIN 或 LOGIN 认证（强制 TLS） |
| 自动发现 | 内置国内外常见服务商预设 → Thunderbird ISPDB → 域名 autoconfig → MX 推断 → 手动填写 |

- **Client ID 注入**：编译时通过环境变量 `NUNCIO_GOOGLE_CLIENT_ID`、`NUNCIO_GOOGLE_CLIENT_SECRET`、`NUNCIO_MS_CLIENT_ID` 注入（Rust `option_env!`），不写进源码。本地开发放在 `.env.local`（已加入 gitignore），CI 用 GitHub Actions secrets。
- **自定义 Client ID**：高级设置里允许填写自己的 Client ID，用户多了以后可以绕开 Google 未验证应用的 100 人上限。
- 申请流程见附录 A（Google）和附录 B（Microsoft）。

---

## 8. 安全与隐私

- **邮件 HTML**：Rust 端用 ammonia 白名单清洗，去掉 script、表单、事件属性、`javascript:` 链接。`cid:` 改写为自定义协议 `nuncio://cid/...`。远程图片默认拦截，用户可以按发件人或域名加入信任。
- **渲染隔离**：正文放进 `<iframe sandbox srcdoc>`，不给 `allow-scripts` 和 `allow-same-origin`，并加 CSP，邮件内容无法访问 Tauri IPC。链接点击由外层拦截，确认后用系统浏览器打开。
- **Tauri capabilities**：只给主窗口最小权限，不开放文件系统和 shell 的通配权限。
- **附件**：打开可执行类附件（exe/bat/cmd/ps1/vbs/js/msi/scr/lnk 等）前先警告。在 Windows 上保存附件时写入 Mark-of-the-Web（`Zone.Identifier`），让 SmartScreen 能正常拦截。
- **凭据**：全部存放在系统 keyring 里，不进数据库，也不写日志。
- **网络**：只连用户配置的服务商，外加可选的 ISPDB 查询（会把邮箱域名发给 Mozilla，设置里可以关掉）和自动更新检查（GitHub Releases）。

---

## 9. UI / UX 规范

- **整体**：三栏布局（导航 / 列表 / 内容），左栏可折叠成汉堡菜单，底部是模块切换栏。Win11 下用 Mica 背景，macOS 用 vibrancy。
- **字体**：Windows 用 Segoe UI Variable + Microsoft YaHei UI；macOS 用 SF + PingFang SC；Linux 用系统 sans。
- **邮件**：左栏顶部是"新邮件"按钮，下面依次是账户列表（带未读数）、文件夹（可展开"更多"）。列表区顶部有搜索框、同步按钮、多选、筛选。阅读区顶部是工具栏（回复/全部回复/转发/归档/删除/旗标/更多）。撰写时**在右侧栏内嵌打开**，可以弹出成独立窗口。
- **日历**：左侧是迷你月历和日历列表；顶部是"今天"按钮和视图切换；主区域用 CSS Grid 手写，不引入 FullCalendar 这类大库。
- **联系人**：列表 + 详情两栏。
- **待办**：三栏：智能列表 / 任务列表 / 详情。
- **设置**：从右侧滑出面板，和老版 Mail 一致。
- **快捷键**：Ctrl+N 新建、Ctrl+R 回复、Ctrl+Shift+R 全部回复、Ctrl+F 转发、Ctrl+Q 标为已读、Ctrl+U 标为未读、Insert 旗标、Del 删除、Ctrl+E 搜索、F5 同步、Ctrl+1~4 切换模块。macOS 上 Ctrl 换成 Cmd。
- **无障碍**：键盘可完整操作，焦点可见，高对比度模式下可用。

---

## 10. 系统集成

| 功能 | Windows | macOS | Linux |
|---|---|---|---|
| 托盘 / 菜单栏 | ✓ | ✓ | ✓（依赖桌面环境） |
| 通知 | Toast（点击跳转到具体邮件需要注册 AUMID，待验证） | 通知中心 | libnotify |
| 未读角标 | 任务栏 overlay icon | Dock badge | 依赖桌面环境 |
| 开机自启 | ✓ | ✓ | ✓ |
| mailto | 安装器写注册表 Capabilities，用户在"默认应用"里选择 | Info.plist 声明 URL scheme | `.desktop` 文件中的 MimeType |
| 文件关联 | `.eml` / `.ics` / `.vcf` | 同左 | 同左 |
| webcal:// | deep-link | deep-link | deep-link |
| 自动更新 | tauri-plugin-updater，读取 GitHub Releases 的 `latest.json` | 同左 | AppImage 可用 |

---

## 11. 工程与发布

### 11.1 目录结构
```
nuncio/
├─ index.html  package.json  vite.config.ts  tsconfig.json
├─ src/                    React 前端
│  ├─ app/                 Shell、路由、主题
│  ├─ modules/{mail,calendar,people,todo,settings}/
│  ├─ components/          Fluent 风格基础组件
│  ├─ ipc/                 类型化 IPC（tauri-specta 生成）
│  ├─ i18n/  styles/
├─ src-tauri/
│  ├─ Cargo.toml           workspace
│  ├─ src/                 app 层：commands、窗口、托盘、通知
│  ├─ crates/{core,auth,mail,pim,ical}/
│  └─ tauri.conf.json
├─ docs/PLAN.md
└─ .github/workflows/release.yml
```

### 11.2 构建与发布
- Release profile：`opt-level="s"`、`lto=true`、`codegen-units=1`、`strip=true`、`panic="abort"`。
- CI：参见本仓库的 `.github/workflows/release.yml`。推送 tag `v*` 时触发，矩阵如下：
  - windows-latest：`x86_64-pc-windows-msvc`，只出 NSIS
  - macos-latest：aarch64 和 x86_64 各一份
  - ubuntu-22.04：deb + AppImage
- 版本号：按仓库约定，在最新 tag 基础上小版本 +1，例如 v0.0.1 → v0.0.2。
- **签名**：
  - Windows 没有代码签名证书时，首次运行会弹 SmartScreen "未知发布者"。之后可以考虑 Azure Trusted Signing（按月收费，具体价格待查）。
  - macOS 没有 Apple 开发者账号（99 美元/年）就无法公证，用户需要右键打开或执行 `xattr -cr`（仅限从官方 Releases 下载的可信安装包）。

### 11.3 测试
- Rust 单元测试：
  - MIME 样本集：各种编码、畸形邮件、超大附件
  - ammonia 清洗：XSS 用例集
  - RRULE 展开：时区、夏令时、例外
  - CJK bigram 分词
- 集成测试：Docker 起本地服务：Dovecot（IDLE/CONDSTORE/QRESYNC）、GreenMail（SMTP）、Radicale（CalDAV/CardDAV）。
- 前端：用 Vitest 测核心逻辑；关键流程手工验证，并附截图。

---

## 12. 里程碑（1 人 + AI，合计约 15~17 周，我不确定，日历重复事件和会议邀请是主要变数）

| 阶段 | 周期 | 交付内容 | 验收标准 |
|---|---|---|---|
| M0 准备 | 0.5 周 | 申请 OAuth（附录 A/B）、初始化仓库、CI 骨架 | 推 tag 后三平台都能出包 |
| M1 骨架 | 2 周 | 三栏 Shell、主题/Mica、SQLite 与迁移、IMAP 密码登录、文件夹树、虚拟列表、安全阅读 | 能登录 QQ/163，浏览 1 万封邮件不卡 |
| M2 邮件闭环 | 3 周 | 增量同步 + IDLE、OpQueue、SMTP/发件箱、撰写/回复/转发/草稿、附件、签名、别名 | 断网时操作，联网后正确回放；新邮件 5 秒内出现 |
| M3 邮件体验 | 2 周 | OAuth（Google/Microsoft）、合并收件箱、会话视图、FTS5 中文搜索、通知/托盘/角标、快捷键、规则 | 能用 Gmail 和 Outlook 收发；两个字的中文词能搜到 |
| M4 日历 | 3 周 | Google/Graph/CalDAV 适配器、5 种视图、事件编辑、RRULE、提醒、ICS 邀请回复 | 和网页版日历双向一致；提醒准时弹出 |
| M5 联系人 | 1.5 周 | People/Graph/CardDAV、编辑、分组、vcf 导入导出、收件人补全 | 双向同步一致 |
| M6 待办 | 1.5 周 | Google Tasks/Graph To Do/VTODO、智能列表、子步骤、提醒、重复 | 双向同步一致；能力不支持时正确降级 |
| M7 集成与发布 | 1.5 周 | mailto/eml/ics/vcf 关联、打印、自动更新、备份恢复、设置页完善、i18n | 达到第 13 节的体积和内存指标 |
| 后续可选 | — | POP3、Outlook 的 Graph 邮件适配器（针对关闭 IMAP 的企业租户）、QRESYNC、S/MIME/PGP、数据库加密 | — |

---

## 13. 体积与资源预估（M1 后实测校准）

**安装包**

| 产物 | 预估 |
|---|---|
| Rust 主程序（内嵌约 1.5 MB 前端资源） | 14~20 MB |
| **Windows NSIS（x64）** | **7~11 MB**（不打包 WebView2，Win11 自带） |
| Windows 安装后占用 | 18~25 MB |
| macOS `.dmg`（单架构） | 8~12 MB |
| Linux `.deb` / `.AppImage` | 8~12 MB / 75~95 MB |

**运行时**（Win11，3 个账户，1 万封邮件，日历/联系人/待办各数百条）

| 场景 | 内存（总工作集） | CPU |
|---|---|---|
| 窗口打开、空闲 | 100~170 MB（Rust 25~45 MB，其余是 WebView2 的多个进程） | ≈0% |
| 关窗到托盘 | **18~40 MB** | ≈0% |
| 打开重型 HTML 邮件 | 临时多出 30~80 MB | 短暂峰值 |
| 首次同步 | 多出 20~50 MB | 单核短时间跑高 |
| 冷启动到首屏 | < 1 s | — |
| 磁盘 | 元数据加索引每封约 2~3 KB；正文缓存默认 ≤ 500 MB；附件按需下载 | — |

**v0.1.0 实测（2026-11-13，macOS arm64 release，无账户）**

| 项 | 实测 |
|---|---|
| 主程序（内嵌前端 JS 1.5 MB + CSS 167 KB） | 17 MB |
| `.dmg`（UDZO / LZMA） | 8.7 MB / 6.1 MB |
| 主程序 xz -9（参照 NSIS LZMA 压缩率） | 5.5 MB |
| 窗口打开时内存（phys_footprint：主进程 40 + WebContent 46 + GPU 20 + Networking 7） | ≈113 MB |

**测量方法**：用 PowerShell 汇总 `nuncio.exe` 和它所有 `msedgewebview2.exe` 子进程的工作集；以任务管理器的"专用工作集"作为对照。

---

## 14. 风险与待验证项

| # | 风险 | 应对 |
|---|---|---|
| 1 | Gmail 的 `mail.google.com` 是受限 scope：未验证应用累计最多 100 个用户，登录时有警告 | 自用够了；提供自定义 Client ID 选项；公开发布前再评估是否做 CASA 安全评估 |
| 2 | 企业 M365 租户可能关闭 IMAP / SMTP AUTH，或禁止用户同意未验证发布者的应用 | 个人 Outlook.com 不受影响；企业场景后期加 Graph 邮件适配器 |
| 3 | `async-imap` 对 CONDSTORE/QRESYNC 支持不全 | 手写原始命令和响应解析；必要时换 `imap-next` |
| 4 | Gmail 是否支持 CONDSTORE、Outlook IMAP 是否支持 CONDSTORE、Outlook 发信后是否自动存"已发送" | M1/M2 期间实测，按结果调整同步策略 |
| 5 | Windows 非 MSIX 应用的 Toast 点击跳转 | 验证 tauri-plugin-notification；不行就换 `tauri-winrt-notification` 并注册 AUMID |
| 6 | iCloud 新版"提醒事项"无法通过 CalDAV 访问 | 待验证；不行就只支持 iCloud 日历 |
| 7 | WebView2 内存下限约 60~100 MB | 关窗时销毁 WebView |
| 8 | 安装包无签名导致 SmartScreen 和 Gatekeeper 警告 | 先在 README 说明；后续再买证书 |

---

## 附录 A：Google OAuth 申请流程

> 控制台的菜单名称会随改版变化，以实际界面为准。标注"我不确定"的地方请以 Google 官方文档为准。

**A.0 前置**：一个**个人** Google 账号。不要用公司或学校的 Workspace 账号，以免受组织策略限制。

**A.1 创建项目**
1. 打开 <https://console.cloud.google.com>
2. 顶部项目选择器 → 新建项目 → 项目名 `nuncio`，组织选"无组织" → 创建
3. 确认顶部已经切换到 `nuncio` 项目

**A.2 启用 API**
1. 菜单 → API 和服务 → 库（APIs & Services → Library）
2. 分别搜索并启用：**Google Calendar API**、**Google People API**、**Google Tasks API**
3. Gmail API 不用启用（邮件走 IMAP）。我不确定 XOAUTH2 是否依赖它，如果授权后 IMAP 登录报错，再回来启用

**A.3 配置同意屏幕（Google Auth Platform）**
1. 菜单 → Google Auth Platform → 品牌塑造（Branding）。如果提示"尚未配置"，点 **开始（Get started）**
2. 应用信息：应用名称 `Nuncio`，用户支持邮箱填你的邮箱 → 下一步
3. 受众群体（Audience）：选 **外部（External）** → 下一步
4. 联系信息：填你的邮箱 → 下一步
5. 完成：勾选同意《Google API 服务用户数据政策》→ 继续 → 创建
6. Branding 页的首页、隐私政策链接、授权网域先空着。**先不要上传 logo**：上传 logo 可能会触发品牌验证（我不确定当前规则，保险起见不传）

**A.4 添加 scope**
1. Google Auth Platform → 数据访问（Data Access）→ 添加或移除范围（Add or Remove Scopes）
2. 在列表里勾选，或者在底部"手动添加范围"里粘贴以下内容 → 添加到表格 → 更新 → **保存**

| scope | 级别（以控制台显示为准） | 用途 |
|---|---|---|
| `openid` | 非敏感 | 身份 |
| `https://www.googleapis.com/auth/userinfo.email` | 非敏感 | 获取邮箱地址 |
| `https://www.googleapis.com/auth/userinfo.profile` | 非敏感 | 获取昵称、头像 |
| `https://mail.google.com/` | **受限** | IMAP/SMTP 收发邮件 |
| `https://www.googleapis.com/auth/calendar` | 敏感 | 日历读写 |
| `https://www.googleapis.com/auth/contacts` | 敏感 | 联系人读写 |
| `https://www.googleapis.com/auth/contacts.other.readonly` | 敏感 | "其他联系人"，用于收件人补全 |
| `https://www.googleapis.com/auth/tasks` | 敏感 | 待办读写 |

**A.5 创建桌面客户端**
1. Google Auth Platform → 客户端（Clients）→ **创建客户端（Create client）**
2. 应用类型：**桌面应用（Desktop app）**，名称 `Nuncio Desktop` → 创建
3. 在弹窗里**立即**复制 Client ID（`xxxx.apps.googleusercontent.com`）和 Client secret（`GOCSPX-...`），或者点"下载 JSON"
   - 据我所知，新建客户端的 secret 之后不能再完整查看（我不确定是否所有项目都这样），务必当场保存
4. 桌面应用不需要配置重定向 URI。Nuncio 使用 `http://127.0.0.1:<随机端口>` 回环地址接收回调，Google 对桌面客户端默认允许
5. 对桌面应用来说，secret 本来就会被嵌进客户端，Google 官方文档也明确说这种情况下它"不被视为机密"

**A.6 发布到生产（推荐）**
1. Google Auth Platform → 受众群体（Audience）→ 发布状态显示"测试中（Testing）"→ 点 **发布应用（Publish app）** → 确认
2. 控制台会提示需要验证。**不用提交验证**，保持"未验证"状态即可
3. 效果：
   - 任何 Google 账号都能登录，但会看到"Google 尚未验证此应用"，点"高级"→"转到 Nuncio（不安全）"即可继续
   - 出现过未验证提示的新用户，累计上限 100 人
   - refresh token 不会再 7 天过期
4. 如果想先留在测试模式：在 受众群体 → 测试用户 里加上自己的 Gmail 地址。代价是 refresh token 7 天过期，每周要重新登录一次

**A.7 把凭据交给项目（不用发到聊天里）**
- 本地：在项目根目录新建 `.env.local`（会加入 `.gitignore`）
  ```
  NUNCIO_GOOGLE_CLIENT_ID=xxxx.apps.googleusercontent.com
  NUNCIO_GOOGLE_CLIENT_SECRET=GOCSPX-xxxx
  ```
- CI：GitHub 仓库 → Settings → Secrets and variables → Actions → New repository secret，按上面的名字各建一个

**A.8 注意事项**
- Workspace 账号的管理员可能禁止访问未验证应用
- 修改 Google 账号密码后，含 Gmail scope 的 refresh token 会失效，需要重新登录
- 每个 Google 账号在每个 Client ID 下最多保留 100 个有效 refresh token，超出后最旧的会被作废
- 以后增加新 scope 时，要先到"数据访问"里加上，否则会触发额外的警告

---

## 附录 B：Microsoft Entra 应用注册流程

**B.0 前置**
- 官方要求有一个 Azure 账号和 Entra 租户。用个人 Microsoft 账号（outlook.com / hotmail）注册免费 Azure 账号后，会得到一个 "Default Directory"
- 注册时可能需要手机号和信用卡验证，但不扣费，注册应用本身免费。个人账号据说已经不能在"目录外"注册应用（我不确定当前规则）
- 不建议用公司租户：权限受限，离职后也会失效

**B.1 注册应用**
1. 打开 <https://entra.microsoft.com> → Entra ID → 应用注册（App registrations）→ **新注册（New registration）**
2. 名称：`Nuncio Desktop`
3. 支持的账户类型：**Any Entra ID Tenant + Personal Microsoft accounts**（任何组织目录中的账户和个人 Microsoft 账户）
4. 重定向 URI：平台选 **公共客户端/本机（移动和桌面）**（Public client/native），值填 `http://localhost`
5. 点注册 → 在概述页复制 **应用程序(客户端) ID**（一个 GUID）

**B.2 身份验证（Authentication）**
- 确认"移动和桌面应用程序"平台下已经有 `http://localhost`。Entra 对 localhost 忽略端口，Nuncio 用随机端口
- "允许公共客户端流"保持 **否**（我们用授权码 + PKCE，不需要它）
- **不要创建 client secret**：桌面应用属于公共客户端，用不到

**B.3 API 权限（API permissions）**
1. 添加权限 → Microsoft Graph → **委托的权限（Delegated）**，勾选：

| 权限 | 用途 |
|---|---|
| `offline_access` `openid` `email` `profile` `User.Read` | 基础身份和 refresh token |
| `IMAP.AccessAsUser.All` | IMAP 收信 |
| `SMTP.Send` | SMTP 发信 |
| `Calendars.ReadWrite` | 日历 |
| `Contacts.ReadWrite` | 联系人 |
| `Tasks.ReadWrite` | 待办（Microsoft To Do） |
| `MailboxSettings.Read`（可选） | 时区、自动回复状态 |

2. 点添加权限。个人账号第一次登录时自己点同意就行，不需要"授予管理员同意"

**B.4 运行时说明（开发实现用）**
- 一个 access token 只对应一个资源，所以需要两种 token：
  - 邮件：`https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access`
  - Graph：`https://graph.microsoft.com/Calendars.ReadWrite https://graph.microsoft.com/Contacts.ReadWrite https://graph.microsoft.com/Tasks.ReadWrite offline_access`
- 首次授权时一次性同意全部权限，之后用同一个 refresh token 分别换取这两种 access token
- 服务器：IMAP `outlook.office365.com:993`（SSL）；SMTP `smtp.office365.com:587`（STARTTLS）
- XOAUTH2 格式：`base64("user=" + email + "\x01auth=Bearer " + token + "\x01\x01")`

**B.5 把凭据交给项目**
- `.env.local` 写入 `NUNCIO_MS_CLIENT_ID=<GUID>`；CI secrets 也建一个同名的

**B.6 注意事项**
- 公司/学校的 M365 账号：租户可能禁止普通用户同意"未验证发布者"的应用，也可能关闭了 IMAP / SMTP AUTH，这时需要管理员批准（各租户的默认策略不同，我不确定）
- 发布者验证需要 Microsoft 合作伙伴 ID，个人使用可以不做

---

## 附录 C：参考资料

- Wino Mail：<https://github.com/bkaankose/Wino-Mail>
- Google OAuth 2.0（refresh token 过期规则、已安装应用）：<https://developers.google.com/identity/protocols/oauth2>
- Google 配置同意屏幕：<https://developers.google.com/workspace/guides/configure-oauth-consent>
- Google 未验证应用：<https://support.google.com/cloud/answer/7454865>
- Microsoft IMAP/POP/SMTP OAuth：<https://learn.microsoft.com/en-us/exchange/client-developer/legacy-protocols/how-to-authenticate-an-imap-pop-smtp-application-by-using-oauth>
- Microsoft Entra 注册应用：<https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-register-app>
- async-imap：<https://github.com/chatmail/async-imap> · imap-codec：<https://github.com/duesee/imap-codec>
