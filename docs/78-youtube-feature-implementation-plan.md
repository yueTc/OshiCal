# YouTube 平台功能实现计划

- 状态：第二轮并行实现完成，待 CI 与外部环境验收
- 分支：`youtube`
- 工作区：`D:\work\guanrenchi`
- 约束：禁止在当前工作区执行任何构建、测试、编译命令；只做源码编辑和静态分析，验证统一走 CI。
- 既有无关改动：`video/oshical-promo/**`、`docs/67-*`、`docs/71-*` 等现有改动不得覆盖、回退或纳入本功能。

## 1. 最终需求

1. B 站和 YouTube 使用独立主播卡片，不做跨平台人物合并。
2. 支持 YouTube 频道搜索、URL/handle/频道 ID 添加、关联账号。
3. 支持公开社区帖和会员限定社区帖抓取；无会员权限或登录态失效时跳过会员帖，不报错。
4. 复用现有 LLM 文本/图片分析生成日程。
5. 支持 YouTube 预约直播日程、开播/下播检测、开播通知、最近直播和直播场次。
6. 支持订阅者数、当前观看人数、峰值/平均观看人数、直播时长、弹幕数、Super Chat/Super Sticker、会员数量。
7. 每个最终用户填写自己的 YouTube API Key。
8. 接受 Google OAuth，用于官方直播聊天和 Super Chat 接口。
9. 接受加密保存 YouTube 网页会话 Cookie，用于会员限定社区帖。
10. 数据库升级自动迁移，旧 B 站数据默认标记为 `bilibili`。
11. 设置页最下方增加 YouTube 总开关，默认关闭。
12. 关闭总开关时不发起任何 YouTube 请求，隐藏 YouTube 入口和卡片，但保留已有数据和凭据。

## 2. 功能开关

新增设置字段：

```text
AppSettings.youtube_enabled: bool = false
ServerSettings.youtube_enabled: bool = false
```

约束：

- 关闭时：
  - 隐藏主播库平台切换和 YouTube 添加入口。
  - 隐藏现有 YouTube 主播卡片。
  - 停止 YouTube 直播探测、社区帖抓取、统计采样和 OAuth 刷新。
  - 后端搜索、添加、关联、OAuth、抓取接口统一返回 `YOUTUBE_FEATURE_DISABLED`。
  - 不删除任何 YouTube 数据、API Key、OAuth refresh token 或网页会话。
- 开启时：
  - 恢复 YouTube 卡片和后台任务。
  - 展示 API Key、OAuth、网页会话配置区。
- 设置页插入点：`src/components/SettingsView.tsx` 中现有 “B 站登录与 Cookie” 卡片之后，作为最后一个设置卡片。

## 3. 平台模型

新增平台枚举：

```text
Platform::Bilibili
Platform::Youtube
```

为兼容现有数据和减少全仓改名，继续保留 `mid` 字段，语义改为“平台内账号 ID”。

新增或扩展字段：

```text
AccountSearchResult.platform
AccountSearchResult.handle
AccountSearchResult.profile_url
AccountSearchResult.live_url

Streamer.platform
Streamer.handle
Streamer.profile_url
Streamer.uploads_playlist_id

LinkedAccount.platform
LinkedAccount.handle
LinkedAccount.profile_url

LiveSnapshot.platform
LiveSnapshot.live_video_id
LiveStatus.platform
LiveStatus.live_video_id
```

数据库迁移：

```sql
ALTER TABLE streamers ADD COLUMN platform TEXT NOT NULL DEFAULT 'bilibili';
ALTER TABLE streamers ADD COLUMN handle TEXT;
ALTER TABLE streamers ADD COLUMN profile_url TEXT;
ALTER TABLE streamers ADD COLUMN uploads_playlist_id TEXT;
ALTER TABLE linked_accounts ADD COLUMN platform TEXT NOT NULL DEFAULT 'bilibili';
ALTER TABLE linked_accounts ADD COLUMN handle TEXT;
ALTER TABLE linked_accounts ADD COLUMN profile_url TEXT;
```

迁移要求：

- 启动时自动执行，事务内完成。
- 迁移前生成数据库备份。
- 迁移失败回滚并阻止继续启动，保留原库。
- 旧数据全部默认为 `bilibili`。
- 同步 schema 从 2 升到 3；新字段使用 `serde(default)` 兼容旧数据读取。

## 4. YouTube 账号与凭据

### 4.1 API Key

- 用户自行填写。
- 桌面端使用现有本地密钥存储机制。
- 服务端使用 `SecretVault` 加密保存。
- 接口只返回 `configured`，永不回显原文。
- 不提供服务端 `OSHICAL_YOUTUBE_API_KEY` 运行时入口；API Key 必须由最终用户在设置页填写并加密保存。

### 4.2 Google OAuth

用途：

- `liveChatMessages.list/streamList`
- `superChatEvents.list`
- 直播聊天事件统计

要求：

- 使用 OAuth 2.0，保存加密 refresh token。
- 桌面端使用系统浏览器 + 回环地址回调。
- 服务端使用公开回调地址：`/api/youtube/oauth/callback`；非回环地址必须使用 HTTPS，只有 localhost/127.0.0.1/[::1] 允许 HTTP。
- 提供授权、刷新、退出、状态查询。
- OAuth 失效时停止聊天统计并提示重新授权。

### 4.3 网页会话

用途：

- 会员限定社区帖抓取。

要求：

- 独立于 API Key 和 OAuth。
- 优先使用受控 WebView 登录并捕获必要 Cookie。
- 使用现有加密存储；桌面端保存到 Windows 凭据管理器，服务端保存到 `SecretVault`。
- 提供清除会话。
- 无权限、未登录、会员失效或页面无 Community 标签时返回 `skipped`，不视为错误。

### 4.4 服务端主密钥与凭据迁移

- 显式设置 `OSHICAL_SECRETS_KEY` 时继续兼容现有部署；该值不得提交或写入数据库。
- 未显式设置时，服务端首次启动生成随机主密钥文件，默认位于数据库同目录的 `<数据库文件名>.secrets-key`，也可用 `OSHICAL_SECRETS_KEY_FILE` 指定数据库外路径。
- 主密钥文件必须与数据库一起备份；Unix 权限固定为 `0600`，Windows 使用当前服务账号的受限 ACL。
- 旧版本曾把 API Token 或固定数据库路径作为可预测密钥材料。新版本只保留该旧密钥用于识别和解密历史密文，绝不再用其加密新凭据。
- YouTube API Key、OAuth refresh token、网页 Cookie 读取到旧密文时会自动用随机主密钥重加密；无法创建或修复安全主密钥时，保存/迁移返回明确错误并要求用户在设置页重新保存 YouTube 凭据。
- 旧 B 站 Cookie 和 LLM API Key 的读取保留兼容回退，不会被静默删除；后续显式写入会使用新主密钥。
- 丢失主密钥文件且没有 `OSHICAL_SECRETS_KEY` 时，旧随机密钥密文无法恢复。必须恢复原密钥文件，或让用户重新填写对应凭据。
- 回滚到不认识随机主密钥文件的旧服务端版本前，必须先备份该文件；旧版本不会读取新密钥文件，已迁移的 YouTube 凭据需要在回滚后重新保存。不会自动把密文降级回旧可预测密钥。

## 5. YouTube 外部接口

### 5.1 Data API

```text
GET https://www.googleapis.com/youtube/v3/search
GET https://www.googleapis.com/youtube/v3/channels
GET https://www.googleapis.com/youtube/v3/playlistItems
GET https://www.googleapis.com/youtube/v3/videos
```

用途：

- `search.list`：关键词搜索频道；限制为显式提交，不按每次按键请求。
- `channels.list`：URL/handle/ID 解析，读取 `snippet/contentDetails/statistics`。
- `playlistItems.list`：读取上传列表最近视频。
- `videos.list`：读取 `snippet/liveStreamingDetails`，判断直播、预约、结束时间、观看人数。

配额策略：

- `search.list` 有独立每日配额，必须缓存并限制请求频率。
- 频道解析结果缓存 24 小时。
- 频道元数据缓存 6 小时。
- 直播状态使用公开 `/live` 页面 20-30 秒探测；官方 API 每 5 分钟兜底校验。
- 遇到 429/quotaExceeded 停止本轮并提示。

### 5.2 公开页面抓取

```text
https://www.youtube.com/channel/{channelId}/live
https://www.youtube.com/@handle/community
https://www.youtube.com/channel/{channelId}/community
```

用途：

- `/live`：低成本直播探测。
- `/community`：公开社区帖和登录态会员帖。

解析要求：

- 从 `ytInitialData` 提取社区帖节点。
- 归一化为 `PlatformPost`。
- 支持文本、图片、帖子 ID、发布时间、原帖 URL。
- 会员帖按当前网页会话能力抓取；不可用时跳过。
- 页面结构变化时返回可识别错误，不生成错误日程。

### 5.3 OAuth 接口

```text
GET https://www.googleapis.com/youtube/v3/liveChat/messages
GET https://www.googleapis.com/youtube/v3/superChatEvents
```

统计映射：

- 弹幕：live chat message 数量。
- Super Chat / Super Sticker：保存原币种和原金额，不直接合并为人民币。
- 会员：统计 membership 事件数量；不推断会员收入。
- 当前观看人数：`liveStreamingDetails.concurrentViewers`。
- 峰值/平均：基于在线采样计算。

## 6. 后端模块

新增：

```text
crates/core/src/youtube.rs
```

建议能力：

```text
YouTubeClient::parse_account_input
YouTubeClient::search_channels
YouTubeClient::resolve_channel
YouTubeClient::fetch_channel_metadata
YouTubeClient::fetch_recent_uploads
YouTubeClient::fetch_upcoming_live
YouTubeClient::probe_live_page
YouTubeClient::fetch_community_posts
YouTubeClient::fetch_live_chat_events
```

服务层分流：

```text
search_accounts(keyword, platform)
add_subscription(platform, ...)
add_linked_account(platform, ...)
check_live_all -> bilibili | youtube
refresh_sources -> bilibili only
refresh_youtube -> metadata/community/live
```

现有 `sources_for_streamers` 必须过滤平台，禁止使用 YouTube Channel ID 请求 B 站动态接口。

## 7. 日程数据流

新增通用帖子结构或扩展 `DynamicPost`：

```text
platform
author_mid
author_name
published_at
text
images
source_url
raw_kind
```

YouTube 来源类型：

```text
youtube_community
youtube_live
```

日程映射：

- 社区帖：继续走现有 LLM 文本/图片分析。
- 预约直播：使用 `scheduledStartTime` 生成 `youtube_live` 来源。
- 已开播但无预约：直播状态生成直播场次，不伪造未来日程。
- `SourceKind` 扩展对应 labels 和样式。
- `ScheduleSource` 增加平台字段或通过所属 streamer 的平台推导；优先增加 `source_platform` 保证完整性。

## 8. 服务端与本地接口

### 8.1 搜索

```http
GET /api/search?platform=youtube&q=...
```

### 8.2 添加主播

```http
POST /api/subscriptions
```

新增 `platform`、`handle`、`profileUrl`、`uploadsPlaylistId`。

### 8.3 关联账号

```http
POST /api/subscriptions/{id}/links
DELETE /api/subscriptions/{id}/links/{platform}/{mid}
```

### 8.4 凭据

```http
GET /api/secrets/youtube
PUT /api/secrets/youtube
DELETE /api/secrets/youtube
POST /api/youtube/oauth/start
GET /api/youtube/oauth/callback
POST /api/youtube/oauth/refresh
POST /api/youtube/oauth/logout
GET /api/youtube/session/status
POST /api/youtube/session/start
DELETE /api/youtube/session
```

所有接口复用现有 API Token 鉴权，并强制检查 `youtube_enabled`。

### 8.5 Tauri 命令

```text
search_accounts({ keyword, platform })
add_subscription(input)
add_linked_account({ id, platform, mid })
remove_linked_account({ id, platform, mid })
youtube_status()
set_youtube_api_key({ apiKey })
start_youtube_oauth()
complete_youtube_oauth({ code, state })
clear_youtube_oauth()
start_youtube_web_login()
clear_youtube_web_session()
```

## 9. 前端

修改：

```text
src/types.ts
src/lib/api.ts
src/App.tsx
src/components/CreatorsView.tsx
src/components/SettingsView.tsx
src/lib/syncStorage.ts
```

新增建议：

```text
src/components/YouTubeSettingsPanel.tsx
```

主播库：

- 平台切换：B 站 / YouTube。
- YouTube 搜索使用显式提交，不做逐键 API 搜索。
- 结果展示平台徽标、handle、订阅者数、直播状态。
- 主播卡片显示平台徽标和平台专属统计口径。
- 双击直播中打开直播，否则打开频道主页。

设置页：

- 最下方增加 YouTube 总开关。
- 开启后展示 API Key、OAuth、网页会话三个独立状态区。
- 提供测试连接、授权、退出、清除会话。
- 总开关关闭时整个配置区折叠。

## 10. 同步

- `Streamer`、`LinkedAccount`、`LiveStatus`、`SourceSuppression`、`ScheduleSource` 增加平台字段。
- 同步 schema 升到 3。
- 新字段默认 `bilibili`，旧数据可读。
- 更新 `src/lib/syncStorage.ts` 的实体投影和缓存重建。
- 升级前自动备份数据库。
- 旧客户端连接新服务端时提示升级客户端。

## 11. 安全

- API Key、OAuth refresh token、网页 Cookie 永不进入日志、错误消息、同步 payload 或前端回显。
- 三类凭据独立清除。
- OAuth 使用 PKCE。
- 服务端回调校验 `state`。
- 抓取器禁止执行页面脚本，只做 JSON/HTML 静态解析。
- 会员帖无权限时跳过，不输出敏感内容。
- YouTube API 数据缓存有 TTL，不长期保存聊天原文；只保存统计聚合结果。

## 12. 并行智能体写集

### Agent A：Core YouTube Client

写入范围：

```text
crates/core/src/youtube.rs（新增）
crates/core/src/youtube_tests.rs（可选新增）
crates/core/src/lib.rs（仅新增模块导出）
```

任务：

- Data API 请求和响应映射
- URL/handle/channel ID 解析
- 社区页 `ytInitialData` 解析
- `/live` 页面解析
- 直播聊天/Super Chat 数据映射接口
- 纯静态分析，不执行页面脚本

### Agent B：Core 数据模型与存储迁移

写入范围：

```text
crates/core/src/models.rs
crates/core/src/settings.rs
crates/core/src/store.rs
crates/core/src/store/sync_feed.rs
crates/core/src/store/import.rs（仅平台字段兼容）
```

任务：

- `Platform` 枚举
- 模型字段
- 数据库自动迁移和备份
- 同步 schema 3
- 平台字段的持久化、加载和 payload

### Agent C：Server OAuth、Secret 与 API

写入范围：

```text
server/src/config.rs
server/src/secrets.rs
server/src/api.rs
server/src/runtime.rs
server/src/youtube_oauth.rs（新增）
```

任务：

- 环境变量和 SecretVault
- OAuth start/callback/refresh/logout
- 网页会话状态和清除
- YouTube API 路由
- 总开关后端校验
- 后台任务开关

### Agent D：Desktop Bridge 与前端 UI

写入范围：

```text
src-tauri/src/lib.rs
src/types.ts
src/lib/api.ts
src/lib/syncStorage.ts
src/App.tsx
src/components/CreatorsView.tsx
src/components/SettingsView.tsx
src/components/YouTubeSettingsPanel.tsx（新增）
src/styles.css（仅 YouTube 面板/徽标样式）
```

任务：

- Tauri 命令
- 设置页最下方总开关
- YouTube 凭据配置区
- 主播库平台切换和结果展示
- 同步字段投影
- 开关关闭时隐藏入口和卡片

### Agent E：测试与文档

写入范围：

```text
docs/79-youtube-feature-verification.md（新增）
crates/core/src/youtube.rs（只读，禁止修改，避免与 Agent A 冲突）
server/tests/（仅新增 YouTube 测试文件）
scripts/（仅新增 YouTube fixture/probe 脚本）
```

任务：

- 测试矩阵
- fixture 设计
- CI 验证清单
- 发布前验收步骤
- 安全审计清单

## 13. 集成顺序

1. 先合入 Agent B 的平台模型和迁移。
2. 合入 Agent A 的 YouTube Client。
3. 合入 Agent C 的服务端 OAuth/API。
4. 合入 Agent D 的桌面桥和前端。
5. 由主代理统一处理跨模块编译错误、接口对齐和剩余集成。
6. Agent E 在不修改主实现文件的前提下补充测试和文档。

## 14. 验证与发布

禁止本地构建/测试。CI 至少执行：

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace --no-fail-fast
npm run check
npm run build:web
```

验证重点：

- 旧 B 站数据迁移
- YouTube 总开关关闭时零请求
- API Key 不回显
- OAuth state/PKCE
- 会员帖无权限跳过
- 社区帖文本/图片生成日程
- 开播/下播通知只触发一次
- 现有 B 站功能回归
- 同步 schema 3 往返
- 前端真实入口操作和截图

版本与发布：

- 桌面端：`0.3.23-beta.1`
- 服务端：`0.3.31-beta.1`
- 两端发布说明分别写入 `docs/`
- 正式版必须等用户明确确认后发布
- 测试版不得移动 stable/latest 清单

## 15. 当前假设

- YouTube 总开关默认关闭。
- API Key 由每个最终用户填写。
- 接受 Google OAuth。
- 接受加密保存网页会话 Cookie。
- 会员帖无权限时跳过。
- YouTube 多币种金额保留原币种，不自动折算人民币。
- 同一主播的 B 站和 YouTube 记录保持独立。
## 16. 第二轮并行修复波次（2026-11-12）

当前状态：主功能已完成第一轮源码实现，尚未构建、测试或发布。本轮只处理交付阻断项和跨模块一致性。

### Wave 2.1：LiveSession 同步闭环

写入范围：

```text
crates/core/src/models.rs
crates/core/src/store.rs
crates/core/src/store/sync_feed.rs
src-tauri/src/sync_client.rs
```

任务：

- 把 `membership_count`、`youtube_stats_available` 加入 `LiveSession`、`LiveSessionRow`、所有 `live_sessions` 查询和映射。
- 在 Tauri 端 `apply_live_session` 写入两个新字段。
- 补充同步 payload 往返断言，确认服务端下发的 YouTube 统计字段不会在客户端丢失。

出口校验：全仓静态扫描不存在“新增了列但 SELECT/struct/apply 未覆盖”的路径；相关测试代码包含新字段断言。

### Wave 2.2：YouTube 统计采集正确性

写入范围：

```text
crates/core/src/youtube_stats.rs
crates/core/src/youtube.rs
docs/02-architecture.md
scripts/live-online.test.ts
```

任务：

- collector 使用分页后的 Super Chat 全量读取能力，并设置明确的安全页数上限和去重。
- 保持 OAuth 失效降级语义：不伪造 0，不把失败写成可用统计。
- 补充 YouTube 观看人数指标文档和前端指标测试覆盖。

出口校验：静态检查确认 collector 不再只取单页 Super Chat；OAuth 失败、分页重复 token、事件去重均有代码路径或测试断言。

### Wave 2.3：服务端错误码、凭据与默认值

写入范围：

```text
crates/core/src/error.rs
server/src/api.rs
server/tests/youtube_api.rs
```

任务：

- 所有 `YOUTUBE_` 前缀错误统一透传，不降级为 `EXTERNAL_ERROR`。
- 保持 YouTube 总开关关闭时路由 fail-closed。
- 核对每个最终用户必须自行填写 API Key 的产品约束，避免服务端环境变量静默绕过。
- 统一 YouTube 新主播 `liveStatsEnabled` 默认语义，并补测试。

出口校验：按错误码前缀静态枚举；关闭开关和 API Key 边界有服务端测试断言。

### Wave 2.4：前端开关、平台隔离与访问性

写入范围：

```text
src/App.tsx
src/components/CreatorsView.tsx
src/components/YouTubeSettingsPanel.tsx
src/lib/api.ts
src/lib/live-online.ts
src/types.ts
src/styles.css
```

任务：

- 关闭总开关时不渲染 YouTube 入口、卡片、凭据操作和直播统计。
- YouTube 添加成功后卡片、平台徽标、统计口径保持独立。
- 凭据状态只显示 configured/expired，不显示原文。
- 检查键盘操作、禁用态、错误提示和移动端布局。

出口校验：静态检查所有 YouTube 入口经过开关门控；组件类型与后端字段一致；无敏感值进入 DOM 或 localStorage。

### Wave 2.5：独立审查（只读）

审查范围：YouTube 相关 Core、Server、Tauri、前端、测试和文档。

输出：按 P0/P1/P2 给出文件行号、问题、证据、修复建议；不得修改实现文件，不得执行构建或测试。

## 17. 本轮统一验收门禁

```text
git branch --show-current                 # 必须为 youtube
git diff --check                          # 空白错误必须为 0
rg 静态字段扫描                            # 新增字段的模型/SQL/同步/UI 链路完整
CI: cargo fmt --check
CI: cargo check --workspace
CI: cargo test --workspace --no-fail-fast
CI: npm run check
CI: npm run build:web
CI: node scripts/youtube-fixtures/validate-fixtures.mjs
```

本地禁止执行上述 CI 命令。任何未运行项必须列入未验证项，不能声明为已通过。
## 18. 第二轮修复结果与剩余风险（2026-11-12）

已完成的集成修复：

- `LiveSession.membershipCount`、`youtubeStatsAvailable` 已贯通 Core 模型、SQL 查询、同步 bootstrap/changes、Tauri apply 和前端类型。
- Super Chat 采集改为有页数上限、continuation token 去重和事件 ID 去重的全量分页；Super Sticker 保留原事件类型、原币种和原金额。
- 所有 `YOUTUBE_*` 错误码在 API 边界保留；动态 `YOUTUBE_HTTP_<status>` 不折叠。
- 总开关关闭时，YouTube 刷新循环、统计 tick、聊天分页、Super Chat 分页、桌面 OAuth 回调与网页登录都会停止后续请求。
- 桌面端增加受总开关控制的后台 OAuth 刷新任务，直播聊天统计不再依赖设置页保持打开。
- 服务端 YouTube API Key 只能来自用户加密保存的 SecretVault；环境变量绕过已移除。
- 服务端未显式配置 `OSHICAL_SECRETS_KEY` 时生成随机主密钥文件；旧版可预测密钥仅用于兼容解密和自动迁移，不再用于新写入。
- 中间 beta schema 升级也会在新增 YouTube 统计列、Super Chat 表或索引前生成数据库备份。
- 清除 YouTube 网页会话不再删除通用 Google 账号 Cookie。

仍保留并需在 beta 说明中披露的限制：

- `streamers.mid` 仍为全局唯一，尚未迁移为 `(platform, mid)`；当前 B 站数字 ID 与 YouTube `UC...` ID 实际碰撞概率低。
- 社区页只解析当前首屏 `ytInitialData`，未实现 continuation 多页抓取。
- 预约直播只检查最近 20 个上传，未使用 `search.list?eventType=upcoming` 兜底。
- Super Chat 明细表仍为设备本地子历史；跨设备同步的是场次级聚合字段和可用性状态，原币种明细在纯镜像端可能缺失。
- YouTube 搜索和频道元数据尚无统一 TTL 缓存，频繁显式搜索会消耗用户自己的 API 配额。
- 桌面、服务端和本地命令仍存在三套搜索/添加调用路径，搜索数量与本地结果合并策略尚未完全统一。
- 真实 Google OAuth、YouTube Data API、会员 Cookie、Windows ACL、SQLite 迁移回滚和真实 UI 关闭零请求仍需在 CI 或外部构建机验收。

## 19. Wave 3：多智能体并行审查与收口（2026-11-12）

### 19.1 目标与边界

本 Wave 不再重做已完成的 YouTube 主功能，而是对提交 `f93012b` 做独立审查、修复确定的 P0/P1 缺陷，并把所有结果收敛到 `youtube` 分支。

强制边界：

- 只允许在 `youtube` 分支工作；不得切换分支、提交、推送、发布或合并。
- 禁止在当前主机执行任何构建、测试、编译、格式化或包管理命令，包括 `cargo *`、`npm run *`、`npx tauri *`、`docker build`。
- 不得触碰未纳入本功能的既有改动：`video/oshical-promo/**`、`docs/67-*`、`docs/71-*`、`.ai-memory/**`、`target/**`、`release/**`、`dist/**`。
- 每个智能体只能修改自己的写入范围；发现跨范围问题时只报告，不直接改别人的文件。
- 只修复有明确证据的 P0/P1；P2 默认记录为 beta 已知限制，除非修复不需要扩大影响面。
- 禁止通过放宽断言、删除测试、隐藏未验证项来“通过”审查。

### 19.2 并行任务

#### Agent A：YouTube 解析与统计

写入范围：

```text
crates/core/src/youtube.rs
crates/core/src/youtube_stats.rs
scripts/youtube-fixtures/**
```

审查重点：

- 频道 ID、handle、频道 URL、视频 URL 解析边界。
- Data API 错误、配额、429、分页 token、空响应和页面结构变化。
- `/live` 状态判定不能把未知页面误判为开播。
- 社区帖公开/会员帖解析、会员权限缺失时跳过。
- 直播聊天、Super Chat/Super Sticker、会员事件、去重、分页上限和原币种。
- 总开关关闭后不得继续发起下一页请求。

产出：修复范围内的确定问题；输出文件行号、证据、未修复 P2、静态检查命令与结果。

#### Agent B：数据模型、迁移与同步

写入范围：

```text
crates/core/src/models.rs
crates/core/src/settings.rs
crates/core/src/store.rs
crates/core/src/store/**
crates/core/src/service.rs
crates/core/src/weekly.rs
```

审查重点：

- `Platform` 默认值与旧 B 站数据迁移。
- schema 2 → 3、中间 beta schema、备份、事务回滚。
- `streamers.mid` 唯一约束、平台过滤、关联账号和跨平台隔离。
- `LiveSession.membershipCount`、`youtubeStatsAvailable` 在查询、同步 bootstrap/changes、导入导出中的完整性。
- 关闭总开关后后台刷新/采样不再启动。
- 迁移失败必须 fail-closed，不得静默继续使用半迁移数据库。

产出：修复范围内确定问题；输出迁移路径、回滚路径、静态字段链路证据和未验证项。

#### Agent C：服务端 OAuth、SecretVault 与 API

写入范围：

```text
server/src/api.rs
server/src/config.rs
server/src/runtime.rs
server/src/secrets.rs
server/src/youtube_oauth.rs
server/tests/youtube_api.rs
```

审查重点：

- OAuth PKCE、state 一次性校验、回调重放、token 刷新轮换、logout。
- API Key/OAuth refresh token/网页 Cookie 三类凭据隔离，接口、日志、错误和同步 payload 不回显。
- 主密钥文件生成、权限、旧密钥兼容迁移和失败恢复。
- YouTube 总开关在后端所有路由与后台任务 fail-closed。
- 每个最终用户自行填写 API Key，不存在环境变量绕过。
- 动态 `YOUTUBE_HTTP_<status>` 和其他 `YOUTUBE_*` 错误码不折叠。
- 服务端测试本身可静态编译通过 Type/字段/函数引用检查。

产出：修复范围内确定问题；输出安全审计结论、接口清单、测试覆盖缺口和未验证项。

#### Agent D：桌面桥、前端与交互

写入范围：

```text
src-tauri/src/lib.rs
src-tauri/src/sync_client.rs
src/types.ts
src/lib/api.ts
src/lib/live-online.ts
src/lib/sync.ts
src/lib/syncStorage.ts
src/App.tsx
src/components/CreatorsView.tsx
src/components/SettingsView.tsx
src/components/YouTubeSettingsPanel.tsx
src/styles.css
```

审查重点：

- 设置页最下方 YouTube 总开关，默认关闭；关闭时隐藏入口/卡片并停止 OAuth、网页登录和后台请求。
- B 站与 YouTube 独立卡片、独立统计口径和独立添加流程。
- API Key/OAuth/网页会话状态只显示 configured/expired，不进入 DOM、localStorage 或日志。
- 系统浏览器 OAuth + 回环 callback 的 state、超时、取消和重复回调处理。
- 受控 WebView Cookie 捕获、清除范围不得误删通用 Google 账号 Cookie。
- 前端与 Tauri/Sync 字段类型一致，空值/旧数据/失败态不崩溃。
- 键盘操作、禁用态、错误提示和移动端布局。

产出：修复范围内确定问题；输出 UI 入口静态链路、敏感值扫描结果、未验证的真实 UI 项。

#### Agent E：验证、CI 与发布门禁

写入范围：

```text
docs/79-youtube-feature-verification.md
scripts/youtube-fixtures/validate-fixtures.mjs
```

审查重点：

- 测试矩阵是否覆盖用户需求和已有实现路径。
- CI 命令、fixture 校验、迁移演练、OAuth 真实测试、关闭零请求验证是否可执行。
- 发布说明门禁、beta/stable 节奏、回滚步骤是否正确。
- 找出静态审查能发现但不能替代的真实环境验证项。
- 不修改业务源码，不与 A-D 的测试/实现写集冲突。

产出：补强验证文档和 fixture 校验；输出最终验收清单、缺口类型、发布建议。

### 19.3 集成顺序

1. 五个智能体并行审查；每个先给出问题清单，再只修复自己范围内的确定 P0/P1。
2. 主代理逐个读取结果，核对文件行号、diff 和未验证项，不抽样、不直接采信子代理结论。
3. 主代理统一处理跨范围接口对齐，禁止把跨范围修改偷偷塞进某个智能体范围。
4. 只暂存 YouTube 功能相关文件，先运行 `git diff --check` 和静态 `rg` 扫描，再提交到 `youtube`。
5. CI/外部构建机通过后，才允许进入 beta 发布说明和发布流程。

### 19.4 出口校验

- `git branch --show-current` 输出 `youtube`。
- `git status --short` 中除了既有无关改动，不出现未归属范围的临时文件。
- 每个智能体报告：已完成、相关文件、证据、未完成、阻塞、P0/P1/P2。
- 所有 P0/P1 均有修复 diff 或明确的阻断说明；P2 全部进入 beta 已知限制。
- 静态检查通过不代表构建通过；CI 结果未返回前，状态只能是“待外部验证”。
