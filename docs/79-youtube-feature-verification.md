# YouTube 功能验证、CI 与发布门禁

- 状态：只读静态审查、P1 修复和 CI 收口完成；真实 OAuth、Data API、会员 Cookie、SQLite 迁移回滚、Windows ACL、真实 UI 和 B 站完整回归尚未执行
- 分支：`youtube`
- 当前审查基准：`e7fa3c9`；历史基准：`f93012b`、`3cd76bc`
- CI：run `43153298753` 已全绿（提交 `e7fa3c96e3c8af5b94363b2a84703916709e1394`）
- 对应实施计划：`docs/78-youtube-feature-implementation-plan.md`
- 当前发布结论：CI 门禁通过，stable 仍阻断；beta 可作为外部验收载体，但发布说明与版本门禁仍须完成

> 本文件区分 GitHub Actions 已执行证据与真实外部环境证据。CI 通过不代表迁移、账号授权、真实会员页面、安装包或 B 站端到端回归已经通过。

## 1. 强制边界

- 本只读审查 Wave 内不提交、不推送、不发布；主代理可按用户授权处理提交。功能发布不能直接从 `youtube` 分支执行，必须先完成审核和合并流程。
- 当前主机不得运行 `cargo`、`npm`、`npx`、`docker`、格式化、lint、构建、测试或 Node fixture 校验。
- 构建、测试、lint、fixture 校验、安装升级、OAuth、真实页面和真实 UI 验收必须在 GitHub Actions 或用户明确指定的外部环境执行。
- 本文档和 `scripts/youtube-fixtures/validate-fixtures.mjs` 的修改不得被解释为业务代码已通过验证。
- 发布、版本号、发布说明和回滚必须同时遵守仓库根目录 `AGENTS.md`。

## 2. 证据分级与缺口类型

| 等级 | 含义 |
|---|---|
| 静态实现 | 当前源码中存在对应路径，但未运行 |
| 测试入口 | 仓库中存在单元/集成测试或 fixture 断言，但未执行 |
| CI 执行 | 当前 HEAD 已有可核验的 GitHub Actions 运行结果；只证明自动化步骤通过 |
| 真实环境 | 必须连接真实操作系统、浏览器、Google、YouTube 或 SQLite 文件 |
| 已执行 | 只有拿到可核验的运行链接或日志后才能使用 |

缺口类型固定为：

- **无入口**：没有测试、脚本、故障注入点或可执行验收步骤。
- **无数据**：缺少真实旧库、真实页面、真实凭据或真实频道样本。
- **环境不可达**：当前静态审查环境不能访问 Google、YouTube、Windows WebView 或真实发布清单。
- **未实测**：已有入口，但本次未执行，不能声明通过。

## 3. 当前验证结论

### 3.1 当前发布阻断

1. **已关闭：账号输入及其他源码 P1 已由 CI 复核。**
   - 生产解析器将显式非 YouTube URL 安全降级为关键词搜索，不访问该 URL，也不把它作为上游请求目标。
   - `scripts/youtube-fixtures/account-inputs.json`、`scripts/youtube-fixtures/validate-fixtures.mjs` 和 Rust 回归测试已在 run `43153298753` 同时通过。
   - 同一轮已修复：带平台的关联账号同步删除事件、桌面 OAuth 刷新/退出竞态、YouTube 社区帖稳定指纹、会员预览图片过滤、YouTube 聊天事件持久去重。

2. **当前桌面版本已有一份与 YouTube 无关的发布说明。**
   - `package.json` 和 `src-tauri/tauri.conf.json` 当前为 `0.3.23-beta.1`。
   - `docs/78-release-desktop-0.3.23-beta.1.md` 描述的是“最近直播”功能，并明确写明不包含共享核心数据层改动。
   - 若直接使用当前版本发布 YouTube，Release 工作流会读取这份错误正文。
   - 必须生成新的桌面 beta 版本号和全新发布说明；不可复用或覆盖旧的已发布说明。具体版本以实际正式版/测试版清单计算，当前静态建议为 `0.3.23-beta.2` 或下一可用 beta，仍需发布前核对。

### 3.2 P1 beta 外部验证阻断

| ID | 缺口 | 类型 | 当前证据 | 关闭条件 |
|---|---|---|---|---|
| GAP-MIG-01 | 没有真实旧库经 `SqliteStore::connect_path` 自动迁移的测试 | 无入口/无数据 | 只有备份条件检测单元测试；同步默认值测试不等于 `ALTER TABLE` | 外部环境完成第 6 节迁移演练 |
| GAP-OAUTH-01 | PKCE、state 一次性、token 轮换、refresh/logout 没有真实 Google 链路 | 无入口/环境不可达/未实测 | 现有服务端测试主要覆盖缺配置 fail-closed、feature gate 和错误码 | 第 7 节真实 OAuth 集成通过 |
| GAP-ZERO-01 | 总开关关闭后的“全局零外部请求”未验证 | 无入口/未实测 | 路由短路测试和 collector 分页门闩测试不能证明所有后台任务/浏览器路径为零 | 第 8 节代理或请求计数验收通过 |
| GAP-FIX-01 | 除账号输入外，其余 fixture 未被生产 Rust 测试消费 | 无入口 | 静态 rg 只发现 `account-inputs.json` 被 Rust 读取；Data API、社区、直播、OAuth、legacy 仅由 JS 自检 | 生产解析器读取同一 fixture，或等价的 Rust golden tests 存在并在 CI 执行 |
| GAP-UI-01 | 没有 YouTube 专用真实 UI 自动化 | 无入口/未实测 | 现有浏览器脚本是既有直播图表和卡片回归，不覆盖 YouTube 设置、凭据和独立卡片全流程 | 第 9 节 UI-001 至 UI-016 在安装包/构建产物上通过 |
| GAP-CI-01（已关闭） | `youtube` 分支 CI 已触发并通过 | 已执行 | run `43153298753`，提交 `e7fa3c9`，全部步骤成功 | 保持后续提交同样全绿 |
| GAP-COOKIE-01 | 会员社区帖只验证合成 fixture | 无数据/环境不可达 | 真实 YouTube 页面结构与会员登录态未测试 | 有权限/无权限账号各完成一次真实会员帖验收 |
| GAP-API-01 | 真实 Data API 配额、分页、429、Super Chat continuation 未验收 | 无数据/环境不可达 | 现只有解析器测试和本地统计门闩测试 | 使用专用测试项目和配额预算完成第 7、10 节 |
| GAP-WIN-01 | Windows 主密钥 ACL、系统浏览器回环回调、WebView Cookie 捕获未验收 | 环境不可达 | 仅有源码路径和局部单元测试 | Windows 构建产物上完成凭据与 OAuth smoke test |

### 3.3 P2 已知限制

以下限制可进入 beta 发布说明，但不得被描述成已解决：

- `streamers.mid` 仍是全局唯一，尚未迁移为 `UNIQUE(platform, mid)`。
- 社区页只解析首屏，不抓取 continuation 多页。
- 预约直播只检查最近 20 个上传。
- Super Chat 明细仍以设备本地历史为主，跨设备只同步场次级聚合状态。
- 尚无统一 YouTube 搜索/频道元数据 TTL 缓存。
- 桌面、服务端和本地命令仍存在多套搜索/添加路径。
- 部分测试资产只做 fixture 自检，尚需真实运行。
- 直播聊天、Super Chat 和会员事件依赖用户自己的 OAuth，不可用时应降级而不是伪造零值。
- 聊天/Super Chat continuation 与去重水位目前只在进程内恢复；应用进程重启后，普通弹幕和会员事件可能重新扫描首页并重复计数，Super Chat 由数据库唯一键防重。该限制必须写入 beta 说明。

## 4. 用户需求覆盖矩阵

| ID | 用户需求 | 静态实现路径 | 当前验证入口 | 状态/缺口 |
|---|---|---|---|---|
| REQ-01 | B 站与 YouTube 独立卡片，不合并人物 | `App.tsx`、`CreatorsView.tsx`、平台字段与筛选 | UI-007 | 静态实现；未真实 UI |
| REQ-02 | 搜索、URL、`@handle`、Channel ID 添加 | `crates/core/src/youtube.rs`、三套客户端 API | 账号 fixture、Rust parser test | 契约与 Rust 测试 CI 通过；真实 API 未验证 |
| REQ-03 | 关联账号 | 服务端/Tauri 路由和模型 | 服务端 feature gate 合约 | 新增成功路径未端到端验证 |
| REQ-04 | 每用户填写自己的 API Key | SecretVault/本地密钥存储 | 服务端加密和不回显测试 | 自动化测试 CI 通过；真实用户 Key 未验证 |
| REQ-05 | OAuth 读取直播聊天、Super Chat、会员事件 | `youtube.rs`、`youtube_oauth.rs`、`youtube_stats.rs` | 解析器/错误码/缺配置测试 | 真实 OAuth 无入口 |
| REQ-06 | 网页 Cookie 抓取会员帖 | WebView 捕获、凭据槽位、社区解析 | 合成 fixture | 真实 Cookie/页面未测 |
| REQ-07 | 公开与会员社区帖生成日程 | 社区解析到 LLM/日程链路 | 解析测试、UI-008/009 | 无端到端自动化 |
| REQ-08 | 无会员权限时跳过，不报整轮错误 | `skipped=membership_session_unavailable` | 社区 fixture | fixture 契约 CI 通过；真实会员页面未验证 |
| REQ-09 | 预约直播、开播/下播、最近直播 | `videos.list`、`/live` probe、场次模型 | 视频映射、直播页状态测试 | 真实页面未测 |
| REQ-10 | 开播通知与直播 URL | 通知与 Tauri URL 构造 | UI-011 | 无 YouTube 专用 UI 自动化 |
| REQ-11 | 峰值/平均观看、时长、弹幕、Super Chat、会员数 | 统计 collector、场次聚合、LiveDataModal | store 单测、live-online 前端测试 | 真实聊天/分页未测 |
| REQ-12 | 新主播 `liveStatsEnabled` 默认关闭 | 模型默认值 | `youtube_subscription_defaults_live_stats_to_false` | 自动化测试 CI 通过 |
| REQ-13 | 设置页最下方总开关，默认关闭 | `SettingsView.tsx`、settings 默认值 | 服务端默认关闭测试、UI-001 | 有入口；未真实 UI |
| REQ-14 | 关闭后不发起 YouTube 请求 | 路由 gate、collector gate、OAuth 刷新 gate | 路由/分页有限测试 | GAP-ZERO-01 |
| REQ-15 | 关闭后保留数据和凭据，重新开启可恢复 | settings 切换不删除数据 | 无完整往返测试 | 无入口 |
| REQ-16 | 数据库自动迁移旧 B 站数据 | `store.rs` 备份与迁移 | 备份条件单测、同步默认值 | 单元测试 CI 通过；真实 `connect_path` 迁移未验证（GAP-MIG-01） |
| REQ-17 | 同步协议 schema 3 和旧客户端升级提示 | `sync_feed.rs`、`sync.ts` | schema 3 服务端测试、前端 schema 检查 | 自动化测试 CI 通过；无旧客户端矩阵 |
| REQ-18 | 达到现有 B 站同等核心功能 | 平台分支与共用服务 | CI 全量测试、UI-016 | 全量 CI 通过；真实 B 站与 YouTube UI 回归未验证 |

## 5. Fixture 与测试资产契约

| 文件 | 用途 | 当前消费方 | 结论 |
|---|---|---|---|
| `account-inputs.json` | 账号输入解析 | `server/tests/youtube_api.rs` | 唯一被生产 Rust 测试直接读取 |
| `data-api.json` | Search/Channels/Videos 映射 | `validate-fixtures.mjs` | 需补 Rust production-parser golden test |
| `community.json` | 公开/会员/无权社区页 | `validate-fixtures.mjs` | 需补 Rust production-parser golden test |
| `live-page.json` | 直播/离线/已结束/未知态 | `validate-fixtures.mjs` | 需补 Rust production-parser golden test |
| `oauth-contract.json` | OAuth 能力矩阵 | `validate-fixtures.mjs` | 不是服务端 OAuth 集成测试 |
| `legacy-bilibili.json` | 旧列集默认平台契约 | `validate-fixtures.mjs` | 不是 SQLite 迁移测试 |
| `validate-fixtures.mjs` | 只读 fixture 自检 | CI 的 YouTube fixture contracts 步骤 | 当前 HEAD CI 已通过；不能替代生产测试 |

`validate-fixtures.mjs` 本次增强的静态规则：

- 所有 fixture 的 `fixtureVersion` 必须为 1。
- 频道 ID 必须严格为 24 字符：`UC` 加 22 个合法字符。
- 账号输入必须与生产解析器的 URL/host/裸视频 ID 优先级一致。
- 显式非 YouTube URL 只能作为安全关键词搜索，不得被当作可访问 URL 或上游请求目标；fixture 与生产解析器必须一致。
- 公共社区帖图片按面积选择最大图，而不是依赖数组最后一项。
- 只有明确的会员提示才可返回 membership skip；普通空页不得被误判。
- 直播页有 `videoId` 但缺少 `isLive`/`isLiveNow` 明确信号时必须返回 `YOUTUBE_LIVE_FORMAT`。
- OAuth callback state 不匹配必须优先拒绝，不能因缺少 OAuth 状态而掩盖。
- Legacy fixture 的源行必须严格符合声明的旧列集，并保留 `mid`。

这些规则仍是自检。生产解析器必须直接读取同一 fixture，或维护与其逐字段等价的 Rust golden tests。

## 6. 数据库迁移演练

### 6.1 现有静态证据

- `connect_path` 在 startup 前调用 `backup_before_youtube_schema_migration`。
- `needs_youtube_schema_migration` 检查平台列、直播统计列、Super Chat 表和索引。
- `youtube_schema_backup_detection_covers_intermediate_beta_schema` 只检查“是否需要备份”的判断，不执行真实文件迁移。
- `sync_schema_is_three_and_legacy_bilibili_rows_default_to_bilibili` 验证序列化默认值和同步投影，不验证旧 SQLite 表结构。

### 6.2 外部演练步骤

必须在外部临时目录执行，保留日志和哈希：

1. 准备三份数据库：
   - 旧列集库：无任何 YouTube 平台字段。
   - 中间 beta 列集库：已有部分平台列，缺 `membership_count`、`youtube_stats_available`、Super Chat/聊天事件表或索引。
   - 完整迁移列集库：作为“不应重复备份”的对照。
2. 每份库写入代表性数据：B 站 streamer、linked account、schedule source、suppression、live session、设置和 sync change。
3. 记录迁移前的表清单、列清单、行数、关键行校验和数据库/WAL 路径。
4. 调用生产入口 `SqliteStore::connect_path`，不得用测试函数替代。
5. 断言：
   - 迁移前恰好生成一份 `*.youtube-v3-backup-*.bak`。
   - 备份可独立打开，表、行数和关键字段与迁移前一致。
   - 旧 B 站记录全部为 `platform = bilibili`。
   - 新列、Super Chat 表和索引存在。
   - `membership_count` 与 `youtube_stats_available` 的旧值为 `NULL`，不是伪造的 0/false。
   - 新 schema 二次启动幂等，且不重复生成迁移备份。
   - 全新空库不生成迁移备份。
6. 故障注入：让迁移中任一 `ALTER TABLE`/建表/索引步骤失败，断言事务回滚、进程 fail-closed、原库仍可打开。当前没有公开 failpoint；如无故障注入入口，必须明确标记为“无入口”，不得用正常迁移冒充回滚通过。
7. 恢复演练：停服务，保留当前数据库和主密钥文件，再用迁移前备份恢复；用旧版本打开并做搜索、日程、设置和同步读取。
8. 记录实际 SQL、备份文件名、迁移耗时、日志和操作者。

### 6.3 升级和回滚注意

- 应用内迁移备份使用 `VACUUM INTO`，是一致性数据库快照；手工复制数据库作为外部备份时另行包含 WAL/SHM。只导出 JSON 快照不能视为完整统计库备份。
- 回滚旧服务端前必须备份新的随机主密钥文件。旧版本不读取该密钥文件，已迁移的 YouTube 凭据可能需要重新填写。
- 不得自动把新密文降级回旧可预测密钥。
- 回滚旧客户端时同时恢复对应数据库/服务端版本；`SYNC_SCHEMA_VERSION=3` 的同步 payload/cache 不得按 schema 2 客户端解析。`SERVER_SNAPSHOT_SCHEMA_VERSION` 仍为 1。

## 7. OAuth、Data API 与凭据真实验收

### 7.1 当前自动测试边界

- 缺少 OAuth app 配置时 fail-closed。
- feature gate 在 Google error/code 处理之前生效。
- API Key 加密槽位、响应不回显、同步 payload 不含明文。
- 动态 `YOUTUBE_HTTP_<status>` 不折叠。
- 解析器覆盖聊天文本、Super Chat、Super Sticker 和会员事件。
- 当前 HEAD 的 CI 已执行服务端、fixture、前端和 Rust 测试并通过；真实 Google OAuth 和真实页面仍未执行。

### 7.2 真实 OAuth 验收

使用专用 Google Cloud 项目和测试账号，凭据只放入外部环境 secret：

1. 桌面端系统浏览器发起授权，回环 callback 能完成 code exchange。
2. 授权 URL 使用 PKCE，state 随机且一次性。
3. 重复 callback、篡改 state、缺失 state 均拒绝，且不交换 token。
4. 用户取消或 Google 返回 `access_denied` 时显示可操作错误，不保存半成品 token。
5. refresh token 首次保存后能刷新 access token；轮换后新 refresh token 落盘，旧 token 不被继续使用。
6. logout/revoke 后被撤销 token 不能读取聊天；应用提示重新授权。
7. scope 缺少 `https://www.googleapis.com/auth/youtube.readonly` 时不得声称聊天统计可用。
8. API Key 只能用于公开 Data API，不能替代 OAuth。
9. 服务端 OAuth 回调地址在非回环环境必须是 HTTPS；localhost/127.0.0.1/[::1] 允许 HTTP。
10. 日志、HTTP 响应、DOM、localStorage、同步 payload 中均不得出现 API Key、access token、refresh token、Cookie 或 client secret。

### 7.3 Data API 与聊天验收

- 验证 `search.list`、`channels.list`、`videos.list` 的正常、空结果、400、403、404、429、5xx、非 JSON、超大响应和配额耗尽。
- 验证 upcoming/live/ended/none 映射及 `actualEndTime` 优先级。
- 验证直播聊天 continuation 轮询、切换视频、下播和 token 过期。
- 验证 Super Chat/Super Sticker 多页 continuation、重复 token、重复事件 ID、原币种/原金额。
- 配额预算必须足以覆盖完整分页；失败时保留未知，不写伪造 0/false。

### 7.4 网页会话验收

- 有会员权限账号：会员限定帖子内容进入分析并生成日程。
- 无权限或 Cookie 失效账号：整轮刷新成功，会员帖标记 skipped，不生成错误日程。
- 清除网页会话只删除应用保存的 YouTube 会话 Cookie，不删除通用 Google 账号 Cookie。
- 真实页面结构变化时返回 `YOUTUBE_COMMUNITY_FORMAT` 或保留未知，不能把登录页/同意页当成功空结果。

## 8. 总开关“零外部请求”验收

代码级路由测试只能证明请求在业务处理器前返回；不能证明后台任务、OAuth refresh、WebView 登录或分页 continuation 没有出站请求。

至少选择一种可观测机制：

1. 拒绝全部出站的 HTTP 代理，出现任何 `googleapis.com`、`youtube.com`、`youtu.be` 或 Google OAuth 连接尝试即失败。
2. 可计数的 fake endpoint，断言关闭场景计数为 0。
3. 网络命名空间或防火墙规则拦截上述域名，并保留拒绝日志。

验收步骤：

1. 先开启 YouTube，保存 API Key、OAuth 和网页 Cookie，保留至少一个 YouTube streamer、日程、场次和统计。
2. 关闭总开关并等待超过刷新/采样周期。
3. 分别触发搜索、添加、关联、刷新、OAuth start/refresh、网页会话 start、直播统计和社区抓取入口。
4. 断言上述入口全部返回 `YOUTUBE_FEATURE_DISABLED` 或在前端被阻止。
5. 断言代理/请求计数为 0，包括分页后续请求和后台 OAuth 刷新。
6. 断言主播库隐藏 YouTube 入口和卡片，但数据库、凭据和同步数据仍存在。
7. 重新开启，断言原有数据、凭据、日程和直播统计仍可读取。
8. 桌面端和服务器模式各执行一次；开发模式和打包应用模式至少各执行一次。

仅通过 `server/tests/youtube_api.rs` 或 `youtube_stats.rs` 的门闩测试，不得判定零请求通过。

## 9. 真实 UI、通知与 B 站回归

以下必须在 CI 构建产物或外部 Windows 安装包操作，并同时记录网络请求和截图：

| ID | 操作 | 预期 |
|---|---|---|
| UI-001 | 打开设置页 | 最下方存在 YouTube 总开关，默认关闭 |
| UI-002 | 关闭总开关 | 无 YouTube 入口/卡片，等待完整刷新周期后仍零请求 |
| UI-003 | 开启总开关 | API Key、OAuth、网页会话为三个独立配置区 |
| UI-004 | 保存 API Key | 只显示已配置，不显示原文，刷新/重启后仍只显示状态 |
| UI-005 | 启动并完成 OAuth | 显示已授权/过期状态；撤销后聊天统计停止并提示重授权 |
| UI-006 | 捕获网页会话 | 状态变为已配置；清除后会员帖跳过且不清除通用 Google Cookie |
| UI-007 | 添加 YouTube 频道 | 生成独立 YouTube 卡片，与 B 站卡片不合并 |
| UI-008 | 公开社区帖含日程 | 生成 `youtube_community` 日程 |
| UI-009 | 会员帖无权限 | 跳过，不显示错误日程 |
| UI-010 | 预约直播 | 生成 `youtube_live` 日程 |
| UI-011 | 预约到开播 | 只触发一次通知，点击打开正确 watch URL |
| UI-012 | 直播结束 | 场次结束，峰值/平均/时长/聊天统计可见；未知指标显示未知 |
| UI-013 | 最近直播与场次 | YouTube 场次可打开数据详情，未采集历史不伪造 |
| UI-014 | Super Chat/Super Sticker | 显示原币种/原金额，分页不重复 |
| UI-015 | 会员事件统计 | YouTube 会员数与 B 站舰长数分开显示 |
| UI-016 | B 站回归 | 搜索、添加、动态、直播、通知、日程和统计无新增失败 |

现有浏览器 UI 脚本不包含 YouTube 专用场景；不能以通用前端测试代替上表。

## 10. CI 精确执行清单

### 10.1 当前 CI 配置

以当前 `.github/workflows/ci.yml` 为准，实际包含：

```text
npx tsc -b --pretty false
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
python -m unittest discover -s scripts -p test_refresh_timing.py -v
node scripts/live-schedule-baseline.mjs
npm run test:web
node scripts/youtube-fixtures/validate-fixtures.mjs
cargo test --workspace --locked --no-fail-fast
npm run build:web
```

随后还执行既有 Playwright UI 脚本并上传构建产物。

注意：

- `ci.yml` 已加入 `youtube` push 触发；run `43153298753` 对提交 `e7fa3c9` 全部步骤成功。
- YouTube fixture 步骤和 UI fixture schema 3 已随当前 HEAD 提交；剩余问题不是“未提交”，而是真实环境验收和 UI-001 至 UI-016。
- `cargo check` 没有单独步骤；clippy 会编译，但不能把 `cargo check` 写成“已执行”。
- `npm run check` 出现在 Release 工作流。它包含 `cargo fmt`、clippy 和 TypeScript，不是 CI 当前步骤名。
- 本文件没有在本机执行命令；上述 CI 证据来自 GitHub Actions。

### 10.2 外部验收命令

以下命令只能由 CI 或用户指定外部构建机执行：

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked --no-fail-fast
npx tsc -b --pretty false
npm run test:web
npm run build:web
node scripts/youtube-fixtures/validate-fixtures.mjs
```

需要额外增加但不属于本次可执行范围的门禁：

- 真实 SQLite 迁移与回滚任务。
- 真实 OAuth/Data API 集成任务。
- 零请求代理任务。
- Windows 安装包 smoke test。
- YouTube 专用浏览器/原生 UI 测试。

## 11. Beta 发布与验证门禁

按 `AGENTS.md` 的默认节奏，先发布 beta 作为外部验收载体；真实迁移、OAuth、会员 Cookie、UI 和 B 站回归可在 beta 周期完成，stable 必须等这些验证通过。

### 11.1 Beta 发布前最低门禁

- [x] 当前提交对应 GitHub Actions 全绿（`e7fa3c9`，run `43153298753`）。
- [x] 无已知 P0/P1 源码缺陷；本轮 P1 修复已由 CI 编译和测试。
- [ ] 通过 `npm run version:next-beta -- <desktop|server>` 生成版本号，并核对远端标签。
- [ ] 对应产品 beta 发布说明已定稿，包含功能、迁移、已知限制和回滚方式。
- [ ] 发布工作流能在默认分支执行；不得直接从 `youtube` 分支发布。

### 11.2 Beta 周期验证门禁

- [ ] SQLite 旧列集/中间 beta 列集迁移、备份、二次启动和回滚。
- [ ] 真实 OAuth PKCE/state/refresh/logout、Data API、聊天和 Super Chat 分页。
- [ ] 真实会员 Cookie 和会员帖有权限/无权限页面。
- [ ] 总开关关闭零外部请求，并证明数据与凭据保留。
- [ ] Windows 安装包、WebView、OAuth 回调和主密钥 ACL。
- [ ] UI-001 至 UI-016 在安装包或构建产物上通过。
- [ ] B 站完整回归无新增失败。
- [ ] 所有未验证项和 P2 已知限制写入 beta 发布说明。

### 11.3 Stable 发布前门禁

- [ ] 上述 beta 周期验证全部通过。
- [ ] 同一核心版本从 beta 晋级 stable，例如 `0.3.23-beta.2` 验证通过后晋级 `0.3.23`；不是 beta.N 字符串与正式版完全相同。
- [ ] 正式版发布说明汇总整个 beta 周期的全部用户可见变化。
- [ ] 用户明确确认发布 stable。

## 12. Stable 发布门禁

正式版发布前还必须满足：

- [ ] 同一核心版本的 beta 已完成外部验收；beta 与 stable 的核心版本必须一致，例如 `0.3.23-beta.2` 验证通过后晋级 `0.3.23`。
- [ ] 候选版本严格高于当前正式版清单；测试版渠道同时读取 beta/stable 清单并按 SemVer 选择更高版本。
- [ ] 自动检测和定时更新不会把 beta 降级为较低 stable。
- [ ] 用户主动从 beta 切回较低 stable 时，界面明确提示显式降级并要求二次确认。
- [ ] 数据库迁移备份、主密钥文件、凭据重录和旧客户端/旧服务端回滚均演练通过。
- [ ] 旧客户端连接 `SYNC_SCHEMA_VERSION=3` 服务端时显示升级提示，不静默解析。
- [ ] 发布说明已定稿，Release 正文与标签、渠道、产物完全一致。
- [ ] 用户明确确认发布 stable。
- [ ] 发布后不再为下一批改动前进 PATCH，直到本版本正式发布完成。

## 13. 发布说明与渠道检查

### 13.1 桌面端 beta

必须新建独立的 `docs/<序号>-release-desktop-<version>.md`，至少写明：

- YouTube 总开关默认关闭。
- 每个用户需自填 API Key；OAuth 和网页会话是可选的独立能力。
- 独立 YouTube 卡片、社区帖、会员帖、预约直播、通知、最近直播和统计。
- schema 3 自动迁移、升级前备份、旧 B 站数据默认 `bilibili`。
- 测试版渠道、NSIS 安装包、Tauri 签名和 Windows Authenticode 实际状态。
- 总开关关闭后隐藏入口并停止请求但保留数据。
- 已知限制和回滚方式。

不可复用 `docs/78-release-desktop-0.3.23-beta.1.md`。

### 13.2 服务端 beta

必须新建独立的 `docs/<序号>-release-server-<version>.md`，至少写明：

- 适用的镜像/编译产物、版本号、beta 渠道和固定 `server-beta` 清单。
- schema 3 迁移、SQLite 备份和实际支持范围。
- API Key 不再接受服务端环境变量注入，必须由最终用户设置。
- 随机主密钥文件、旧密钥兼容迁移、ACL/回滚和凭据重录要求。
- OAuth 回调地址、HTTPS/回环限制和 Google 配置步骤。
- 升级前数据库、WAL 和主密钥备份。
- 已知限制和回滚步骤。

### 13.3 渠道与版本

- 桌面候选不能在 `0.3.23-beta.1` 上复用旧说明；以实际发布清单计算新 beta 号。
- 服务端当前静态版本为 `0.3.30`；按本地标签计算的候选为 `0.3.31-beta.1`，发布前仍须核对正式/测试清单并用版本脚本生成。
- 测试版 Release 必须标记 prerelease，并更新固定 `desktop-beta`/`server-beta` 清单；不得移动 `latest` 或正式版 `desktop-v*` 清单。
- beta 发布不自动意味着可以发布 stable；stable 必须等 beta 验证和用户明确确认。
- beta 说明写相对上一 beta 的增量；stable 说明汇总同一核心版本的全部用户可见变化。桌面端和服务端发布说明都必须写明更新渠道、适用产物和升级/回滚要求。

## 14. 回滚检查

- 升级前停止应用/服务端，复制数据库、WAL/SHM、主密钥文件和必要的 OAuth/服务器配置。
- 保留迁移前备份和相关日志；不要只依赖应用内 JSON 快照。
- 回滚服务端时恢复数据库和对应版本产物；旧版本可能无法读取新主密钥文件，YouTube 凭据需重新填写。
- 回滚客户端时恢复旧客户端和对应同步缓存；schema 3 缓存不能强行交给 schema 2 客户端解析。
- OAuth 可撤销测试账号授权；不得删除生产用户凭据作为普通回滚步骤。
- 回滚完成后验证 B 站搜索、日程、直播和设置仍可用，并确认没有把 beta 自动降级给其他用户。
- 任何回滚演练失败都必须阻断 stable；若要发布 beta，必须在 beta 说明中明确写出限制和补救方式。

## 15. 结果报告模板

```text
分支：
提交 SHA / 工作树状态：
CI 运行链接：
fixture 校验结果（含是否命中 PRODUCTION_CONTRACT_MISMATCH）：
cargo fmt / clippy / test：
TypeScript / 前端测试 / Web build：
真实 SQLite 迁移与回滚：
OAuth / Data API / 聊天分页：
会员 Cookie 页面：
总开关零请求：
凭据不回显：
Windows 安装包与 WebView：
真实 UI：
B 站回归：
未验证项：
已知限制：
发布建议：仅 beta / 可 stable / 阻断
```

## 16. 当前交付判断

- 测试矩阵已覆盖主要用户需求，但静态覆盖不等于外部验收通过。
- 静态审查未发现新的 P0；账号输入契约已对齐。仍有迁移、OAuth、零请求、真实 UI 和真实页面等 P1 运行验证缺口。
- 当前 HEAD 的 CI run `43153298753` 已通过；剩余阻断项是真实外部环境验收，而不是 CI。
- P1 源码修复已完成；真实迁移/OAuth/零请求/真实 UI/B 站回归证据齐备前只能是 beta 候选，stable 仍阻断。
- 本文件没有在本机执行 Node fixture 校验、cargo、npm、npx、docker、构建、测试或格式化；CI 证据另见上述运行链接。
