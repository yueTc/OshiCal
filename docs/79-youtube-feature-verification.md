# YouTube 功能验证与测试设计

- 状态：验证资产已设计，未执行
- 分支：`youtube`
- 对应实施计划：`docs/78-youtube-feature-implementation-plan.md`
- 禁止事项：当前工作区不得执行 `cargo build`、`cargo test`、`cargo check`、`cargo clippy`、`npm run build`、`npm run check`、`npx tauri build/dev`、`docker build` 或任何会产生编译产物的命令。
- 执行环境：所有构建、测试、lint、fixture 校验和真实 UI 验收只能通过 GitHub Actions 或用户指定的外部构建机执行。

## 1. 目标

本文件定义 YouTube 功能的可静态审查验证资产和 CI 验收口径，覆盖：

- YouTube 频道 ID、handle、频道 URL、视频 URL 解析。
- YouTube Data API 搜索结果、频道详情、直播/预约/结束状态映射。
- 公开社区帖、会员可访问社区帖、会员权限不可用时的 `ytInitialData` 解析契约。
- `/live` 页面直播、未开播、已结束状态。
- API Key、OAuth、网页会话三类凭据的隔离与不回显。
- 总开关默认关闭、关闭时路由 fail-closed、不得触发 YouTube 外部请求。
- 旧 B 站数据默认为 `bilibili`。
- 同步 schema 升级到 3。
- 现有 B 站功能回归和发布前真实 UI 验收。

## 2. 新增验证资产

| 文件 | 用途 |
|---|---|
| `server/tests/youtube_api.rs` | 服务端 HTTP 契约测试：鉴权、总开关、API Key 加密槽位、OAuth/网页会话不回显、schema 3、B 站默认平台、账号输入 fixture |
| `scripts/youtube-fixtures/account-inputs.json` | 频道 ID、handle、频道 URL、视频 URL、搜索词和非法输入 fixture |
| `scripts/youtube-fixtures/data-api.json` | `search.list`、`channels.list`、`videos.list` 响应与预期映射 |
| `scripts/youtube-fixtures/community.json` | 公开社区帖、会员可访问社区帖、会员权限不可用页面 fixture |
| `scripts/youtube-fixtures/live-page.json` | `/live` 页面直播、未开播、已结束状态 fixture |
| `scripts/youtube-fixtures/oauth-contract.json` | API Key 不等于 OAuth、缺 scope、refresh token 失效、state 不匹配、有效授权场景 |
| `scripts/youtube-fixtures/legacy-bilibili.json` | 旧 B 站 streamer/linked account 行与迁移后的默认平台字段契约 |
| `scripts/youtube-fixtures/validate-fixtures.mjs` | 只读 fixture 契约校验，不联网、不写文件、不产生构建产物 |

`scripts/youtube-fixtures/validate-fixtures.mjs` 只是 fixture 契约自检，不等价于 Rust 生产解析器测试。生产解析器必须由 `crates/core/src/youtube.rs` 的单元测试或集成测试直接读取同一 fixture 进行验证。

## 3. 覆盖矩阵

| ID | 场景 | 验证方式 | 预期结果 |
|---|---|---|---|
| YT-URL-001 | 24 位 `UC...` 频道 ID | fixture + `account_input_fixture_matches_public_parser` | 解析为 `ChannelId` |
| YT-URL-002 | `@handle` 与 `youtube.com/@handle` | fixture + 公共解析器测试 | 解析为规范化 `Handle` |
| YT-URL-003 | `m.youtube.com/@handle/videos` | fixture | 忽略路径后缀，保留 handle |
| YT-URL-004 | `youtube.com/channel/UC...` | fixture | 解析为 `ChannelId` |
| YT-URL-005 | `youtu.be/{id}`、`watch?v={id}`、`live/{id}` | fixture | 解析为 `VideoId` |
| YT-URL-006 | 非 YouTube URL、空 handle | fixture + 公共解析器测试 | 非 YouTube URL 作为关键词搜索；空 handle 返回校验错误 |
| YT-API-001 | `search.list` 频道结果 | fixture 契约 | channelId、标题、头像、主页 URL 正确 |
| YT-API-002 | `channels.list` 频道详情 | fixture 契约 | handle、uploads playlist、订阅数、隐藏订阅数正确 |
| YT-API-003 | `videos.list` 直播中 | fixture 契约 | `live`，当前观看人数 321 |
| YT-API-004 | `videos.list` 预约直播 | fixture 契约 | `upcoming`，保留 `scheduledStartTime` |
| YT-API-005 | `videos.list` 已结束直播 | fixture 契约 | `ended`，保留开始/结束时间 |
| YT-API-006 | 普通视频 | fixture 契约 | `none`，不创建直播状态或日程 |
| YT-COMM-001 | 公开社区帖文本、图片、URL | fixture 契约 | 2 条帖子，图片取最大缩略图 |
| YT-COMM-002 | 会员可访问社区帖 | fixture 契约 | `membershipOnly = true`，内容正常进入 LLM 分析 |
| YT-COMM-003 | 会员权限不可用 | fixture 契约 | `skipped = true`、`skipReason = membership_session_unavailable`，不报错、不生成日程 |
| YT-LIVE-001 | `/live` 重定向到直播视频 | fixture 契约 | `live`，记录 videoId、标题、开播时间 |
| YT-LIVE-002 | `/live` 退回频道页 | fixture 契约 | `offline`，不生成直播卡片 |
| YT-LIVE-003 | `/live` 指向已结束直播 | fixture 契约 | `/live` 只能判定 `offline`；结束时间以 `videos.list` 的 `actualEndTime` 为准 |
| YT-LIVE-004 | 页面有合法 `videoId`，但缺少 `isLive` / `isLiveNow` | `live_page_with_video_id_but_no_live_state_returns_format_error` | 返回 `YOUTUBE_LIVE_FORMAT`，保留未知，不得判离线 |
| YT-LIVE-005 | 页面明确给出 `isLive = false` 或 `isLiveNow = false` | `live_page_with_explicit_not_live_state_is_offline` | 判定 `offline`，不生成直播卡片 |
| YT-MON-001 | `superChatEvents.list` 返回 Super Sticker | `super_chat_events_map_super_sticker_details_and_kind` | 保留 `kind = superStickerEvent`、原币种与金额 |
| YT-OAUTH-001 | 只有 API Key，没有 OAuth | fixture 契约 | 直播聊天、Super Chat、会员事件要求重新授权 |
| YT-OAUTH-002 | OAuth 缺 `youtube.readonly` scope | fixture 契约 | `reauth_required` |
| YT-OAUTH-003 | refresh token 过期/撤销 | fixture 契约 | `reauth_required`，停止聊天统计 |
| YT-OAUTH-004 | callback state 不匹配 | fixture 契约 | 拒绝回调 |
| YT-OAUTH-005 | OAuth 有效且 scope 完整 | fixture 契约 | 允许读取直播聊天 |
| YT-SW-001 | 总开关默认关闭 | `youtube_routes_require_auth_and_are_disabled_by_default` | `youtubeEnabled = false` |
| YT-SW-002 | 关闭时调用搜索、密钥、OAuth、网页会话接口 | `youtube_routes_require_auth_and_are_disabled_by_default` | 返回 `YOUTUBE_FEATURE_DISABLED`，不进入业务处理器 |
| YT-SEC-001 | 保存 API Key | `youtube_api_key_is_encrypted_and_never_returned_or_synced_in_plaintext` | 响应无明文、SQLite 无明文、同步 payload 无明文 |
| YT-SEC-002 | 查询 API Key 状态 | 同上 | 只返回 `configured`，不回显 `apiKey` |
| YT-SEC-003 | OAuth / 网页会话状态 | `youtube_oauth_and_web_session_status_never_expose_credentials` | 不返回 access token、refresh token、cookie、client secret |
| YT-DB-001 | 旧 B 站记录没有平台字段 | `legacy-bilibili.json` + `sync_schema_is_three_and_legacy_bilibili_rows_default_to_bilibili` | fixture 保持旧列集，预期读出为 `platform = bilibili` |
| YT-DB-002 | 中间 beta 已有平台列，但缺 `membership_count`、`youtube_stats_available` 或 `live_super_chat_events` | `youtube_schema_backup_detection_covers_intermediate_beta_schema` + 外部 SQLite 迁移演练 | 启动迁移前生成一致快照备份；迁移后二启动幂等 |
| YT-FE-001 | `LiveSession` 的 YouTube 聚合字段缺失或为 `null` | TypeScript 契约审查 + 后续 API 序列化测试 | `membershipCount` / `youtubeStatsAvailable` 可空；`null` 不等于 0 或 false |
| YT-SYNC-001 | 同步 bootstrap 和 changes | 同上 | `schemaVersion = 3`，streamer payload 含平台 |
| YT-REG-001 | 现有 B 站搜索、添加、动态、直播、通知 | CI 全量回归 + 真实 UI | 无行为变化 |

## 4. 服务端测试设计

### 4.1 `youtube_routes_require_auth_and_are_disabled_by_default`

覆盖接口：

- `GET /api/search?platform=youtube&q=...`
- `GET /api/youtube/search?q=...`
- `GET /api/youtube/status`
- `POST /api/youtube/subscriptions`
- `GET /api/secrets/youtube`
- `POST /api/youtube/oauth/start`
- `POST /api/youtube/oauth/refresh`
- `POST /api/youtube/oauth/logout`
- `POST /api/youtube/session/start`
- `GET /api/youtube/session/status`
- `DELETE /api/youtube/session`

每个接口先断言无 Token 返回 `401`，再断言总开关关闭时返回 `YOUTUBE_FEATURE_DISABLED`。该测试只能证明路由短路顺序；真正的“零外部请求”必须在 CI 通过拒绝全部出站的 HTTP 代理或请求计数器验证。

### 4.2 `youtube_api_key_is_encrypted_and_never_returned_or_synced_in_plaintext`

测试契约：

- `GET /api/secrets/youtube` 的 `data.apiKey.configured` 默认是 `false`，并同时返回独立的 OAuth 与网页会话状态。
- `PUT /api/secrets/youtube` 成功后只返回 `configured: true`。
- `GET /api/secrets/youtube` 不回显 API Key。
- 专用密文槽位为 `server:secret:youtube_api_key:v1`，原始库内容不包含明文或 `apiKey` 字段。
- 所有同步 change payload 不包含明文。
- 删除后槽位和 `configured` 状态都清零。

如果实现采用不同的密文键名，必须同步修改测试和文档；不得绕过密文槽位断言。

### 4.3 `youtube_oauth_and_web_session_status_never_expose_credentials`

测试契约：

- session status 路由存在，不返回 `refreshToken`、`accessToken`、`cookie`、`clientSecret`。
- 测试进程未配置 OAuth app 凭据时，OAuth start 必须 fail-closed，不能打开浏览器流程或返回 token 材料。

完整的 OAuth 成功链路需要可注入的 Google token endpoint。当前服务端若没有测试专用 base URL 注入点，只能放到 CI 沙箱或外部集成环境，不在单元测试中请求真实 Google。

### 4.4 `sync_schema_is_three_and_legacy_bilibili_rows_default_to_bilibili`

测试契约：

- `SYNC_SCHEMA_VERSION == 3`。
- 通过旧字段 JSON 添加 B 站主播，不显式传 `platform`。
- bootstrap 中该主播为 `platform = bilibili`。
- `handle`、`profileUrl`、`uploadsPlaylistId` 为 `null`。
- changes 中 streamer payload 含 `platform = bilibili`。

该测试验证新字段默认值和同步投影，不验证旧数据库文件的真实 `ALTER TABLE` 迁移。真实迁移测试必须在 `crates/core/src/store.rs` 的测试模块执行，因为服务端集成测试没有直接 SQL 依赖。

### 4.5 `account_input_fixture_matches_public_parser`

直接读取 `scripts/youtube-fixtures/account-inputs.json`，调用生产公开解析器 `parse_youtube_account_input`，逐项断言 channel、handle、video、search 分类。非法域名和空 handle 必须返回错误。

## 5. Fixture 设计说明

### 5.1 `account-inputs.json`

- 固定使用 24 位频道 ID 和 11 位视频 ID。
- 覆盖裸 ID、裸 handle、完整 URL、移动端 URL、短链接、`watch` URL、`live` URL。
- 自由文本必须解析为搜索词，不能误判为频道 ID。
- 非 YouTube URL 按关键词搜索处理，避免把第三方 URL 误判为频道；空 handle 必须拒绝。

### 5.2 `data-api.json`

- `search.list` 只返回频道结果。
- `channels.list` 返回 `snippet`、`contentDetails.relatedPlaylists.uploads`、`statistics`。
- `videos.list` 同时包含 live、upcoming、ended、none 四种状态。
- 数值字符串必须显式转成整数，不能把订阅数或观看人数当文本写入统计。
- 已结束直播以 `actualEndTime` 优先判定，不依赖 `liveBroadcastContent = none`。

### 5.3 `community.json`

- `public`：公开帖文本、两张图片中的最大图、相对 `/post/{id}` URL 转绝对 URL。
- `membersOnlyAccessible`：存在 `sponsorsOnlyBadge` 时标记 `membershipOnly = true`，但内容仍可分析。
- `membersOnlyUnavailable`：无帖子且命中会员提示时返回 `skipped`，不得当成解析失败。
- 解析器必须只读 JSON 节点，不执行页面脚本。

### 5.4 `live-page.json`

- `live`：重定向到 `watch?v=...` 且 `isLive = true`。
- `notLiveChannelPage`：最终 URL 没有视频 ID，且页面可识别为频道页，视为 offline。
- `endedBroadcastPage`：`isLive = false` 且 `isLiveContent = true`，`/live` 探测器按 offline 处理；`ended` 状态由 `videos.list` 的 `actualEndTime` 判定。
- `ambiguousVideoId`：存在合法 `videoId` 但缺少 `isLive` / `isLiveNow` 明确状态时必须返回 `YOUTUBE_LIVE_FORMAT` 或保留未知，不能仅凭 `videoId` 判离线。
- 明确离线只接受布尔值 `false` 等显式信号；字段缺失、页面结构未知或验证页都必须保留未知。

### 5.5 `oauth-contract.json`

- API Key 只能访问公开 Data API，不能替代 OAuth 读取直播聊天和 Super Chat。
- 缺 required scope、token 过期、state 不匹配都必须重新授权或拒绝回调。
- 只有有效 OAuth 且 scope 完整时才允许直播聊天能力。

## 6. 总开关零请求验证

代码级测试只能证明总开关关闭时请求在业务入口前返回。完整的零外部请求验收必须在 CI 增加以下任一机制：

1. 启动拒绝所有出站的 HTTP 代理，执行总开关关闭场景；出现任何连接尝试即失败。
2. 在 CI 注入可计数的 YouTube fake endpoint，断言请求计数为 0。
3. 使用网络命名空间或防火墙规则阻断 `googleapis.com`、`youtube.com`、`youtu.be`，再跑关闭场景。

验收标准：

- 关闭开关后搜索、OAuth、网页会话、添加、关联、刷新接口全部返回 `YOUTUBE_FEATURE_DISABLED`。
- 后台直播探测、社区帖抓取、统计采样任务不启动。
- 已保存的 API Key、OAuth refresh token、网页 Cookie 和 YouTube 数据不被删除。
- 重新打开开关后原有数据和凭据仍可用。

## 7. 数据库迁移验证

真实迁移需要验证：

- 旧库启动前自动备份。
- `streamers` 新增 `platform`、`handle`、`profile_url`、`uploads_playlist_id`、`live_video_id`。
- `linked_accounts` 新增 `platform`、`handle`、`profile_url`。
- `schedule_sources` 和 `source_suppressions` 新增 `platform`。
- `live_sessions` 新增 `membership_count`、`youtube_stats_available`，均允许 `NULL`，表示统计尚未可用或旧数据未知。
- 新增 `live_super_chat_events` 子表和 `idx_live_super_chat_events_session` 索引，保留原币种 Super Chat / Super Sticker 明细。
- 启动备份条件必须同时检查上述平台字段、`live_sessions` 新字段和新表/索引；中间 beta schema 也必须在迁移前生成备份。
- 全新空库不应生成多余备份；已有应用 schema 的数据库只要缺少任一必需项就生成备份。
- 旧 `streamers` 和 `linked_accounts` 全部为 `platform = bilibili`。
- 迁移事务失败时回滚，原库仍可打开，进程阻止启动。
- 二次启动幂等，不重复添加列、不覆盖新数据。

建议在 `crates/core/src/store.rs` 测试模块新增 legacy schema fixture：

1. 在临时目录创建只含旧表结构的 SQLite 文件。
2. 插入旧 B 站 streamer、linked account、schedule、live session。
3. 调用 `SqliteStore::connect`。
4. 检查列、默认值、备份文件和二次启动幂等。
5. 从“已有平台列、尚无 YouTube 统计列”的中间 beta schema 升级，断言迁移前备份存在且迁移后数据未丢失。
6. 测试结束清理临时目录。

当前 `server/tests/youtube_api.rs` 不直接依赖 `sqlx`，因此不能安全构造旧表结构；不把默认值测试冒充真实迁移测试。

## 8. 同步 schema 3 验证

至少覆盖：

- `SYNC_SCHEMA_VERSION == 3`。
- `/api/sync/bootstrap` 返回 `schemaVersion: 3`。
- `/api/sync/changes` 返回 `schemaVersion: 3`。
- streamer、linked account、live status、schedule source、suppression 的 payload 都带平台字段。
- 旧客户端遇到 schema 3 时提示升级，不能静默按 schema 2 解析。
- 本地缓存 schema 升级后能重建，旧 B 站条目默认 `bilibili`。
- `LiveSession` 同步投影保留 `membershipCount` 与 `youtubeStatsAvailable`；`null` 表示旧数据或统计未知，不能被前端归一化为 0 / false。
- 回滚时恢复旧数据库和旧客户端版本。

## 9. 前端与真实 UI 验收

以下场景必须在 CI 产物或外部构建机的真实应用入口操作，不允许只用静态断言代替：

| ID | 操作 | 预期 |
|---|---|---|
| UI-001 | 打开设置页 | 最下方存在 YouTube 总开关，默认关闭 |
| UI-002 | 关闭总开关 | 主播库无 YouTube 入口和卡片，后台无请求 |
| UI-003 | 开启总开关 | 显示 API Key、OAuth、网页会话三个独立配置区 |
| UI-004 | 填入 API Key | 状态变为已配置，接口和界面均不回显原文 |
| UI-005 | 撤销 OAuth | 直播聊天统计停止并提示重新授权 |
| UI-006 | 清除网页会话 | 会员帖跳过，不导致整轮刷新失败 |
| UI-007 | 添加 YouTube 频道 | 生成独立 YouTube 卡片，不合并 B 站卡片 |
| UI-008 | 公开社区帖含日程 | 生成 `youtube_community` 来源日程 |
| UI-009 | 会员帖无权限 | 跳过且不显示错误日程 |
| UI-010 | 预约直播 | 生成 `youtube_live` 来源日程 |
| UI-011 | 直播到开播 | 只触发一次开播通知，点击打开正确 watch URL |
| UI-012 | 直播结束 | 结束场次，峰值/平均/时长可查看 |
| UI-013 | B 站现有流程 | 搜索、添加、动态、直播、通知无回归 |

## 10. CI 执行清单

以下命令只能由 CI 或用户指定的外部构建机执行：

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace --no-fail-fast
npm run check
npm run build:web
node scripts/youtube-fixtures/validate-fixtures.mjs
```

建议 CI 单独增加 Windows 构建和真实应用 smoke test。任何本地命令失败都不能用“静态审查通过”替代。

## 11. 安全审计清单

- [ ] API Key 只进加密存储，不进日志、错误、同步 payload、前端状态持久化。
- [ ] OAuth refresh token 只进加密存储，接口只返回 configured/expired 状态。
- [ ] PKCE verifier 和 state 一次性使用，回调校验 state。
- [ ] 网页 Cookie 只进独立加密槽位，不与其他凭据混用。
- [ ] 三类凭据可独立清除，清除一个不影响另外两个。
- [ ] 总开关关闭时后端所有 YouTube 路由 fail-closed。
- [ ] 社区页解析不执行 JavaScript，不把未知 HTML 当成功。
- [ ] 会员帖无权限返回 skipped，不输出帖子内容或 Cookie。
- [ ] 多币种 Super Chat 保留原币种和原金额，不伪造人民币收入。
- [ ] 聊天原文只做短期聚合，不长期写入同步 payload。

## 12. 当前未验证项与已知缺口

本轮只做静态分析和文件编辑，以下项目尚未执行，不能声称通过：

- `server/tests/youtube_api.rs` 尚未编译。
- fixture 校验脚本尚未运行。
- CI 的 `cargo check`、`cargo test`、`npm run check`、Web 构建尚未运行。
- 新增的 Super Sticker、`live` 未知状态和中间 beta 备份条件单元测试尚未执行。
- `youtube_stats.rs` 写入层仍硬编码 `superChatEvent`；解析器已返回 `kind`，后续必须改为使用 `event.kind` 才能完成 Super Sticker 端到端落库。
- 真实 SQLite 旧库迁移、中间 beta 备份文件和回滚尚未通过外部环境演练。
- OAuth 真实回调、PKCE、refresh token 轮换尚未通过外部集成测试。
- 会员社区帖网页会话和真实 YouTube 页面结构尚未验证。
- 真实 UI 的开关、凭据面板、主播卡片、通知和日程流程尚未验收。
- `LiveSession` 的 `membershipCount` / `youtubeStatsAvailable` 尚未通过真实 API 序列化与前端归一化测试。
- 总开关关闭后的外部请求计数尚未通过代理或防火墙验证。
- B 站完整回归尚未运行。

这些项目必须在 CI 或外部构建机完成并在发布说明中如实披露结果。

## 13. 发布前门禁

发布 beta 前必须满足：

- [ ] 服务端集成测试和 core 单元测试在 CI 全绿。
- [ ] fixture 校验在 CI 全绿。
- [ ] schema 3 bootstrap/changes 往返通过。
- [ ] 总开关关闭零请求通过代理或请求计数验证。
- [ ] 三类凭据不回显通过接口和日志检查。
- [ ] 真实 UI 完成 UI-001 至 UI-013。
- [ ] B 站回归无新增失败。
- [ ] 未验证项和已知限制写入对应 beta 发布说明。

正式版发布前还必须满足：

- [ ] 同一核心版本的 beta 已验证。
- [ ] 数据库迁移备份和回滚步骤在外部环境演练。
- [ ] 旧客户端连接新服务端时的升级提示已验证。
- [ ] 用户明确确认发布正式版。

## 14. 结果报告模板

```text
分支：
提交/工作区状态：
CI 运行链接：
fixture 校验结果：
服务端测试结果：
core 迁移测试结果：
同步 schema 3 结果：
总开关零请求验证：
凭据不回显验证：
真实 UI 验收：
B 站回归：
未验证项：
已知限制：
发布建议：仅 beta / 可 stable / 阻断
```
