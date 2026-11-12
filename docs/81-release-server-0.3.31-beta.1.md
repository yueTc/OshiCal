# OshiCal 服务端 0.3.31-beta.1 测试版

本版相对已发布的 `0.3.30` 稳定版及其 `0.3.30-beta.N` 测试周期，加入 YouTube 平台服务和共享核心支持。服务端首次使用新的 YouTube 迁移、OAuth 回调、凭据保管和同步协议；本版仅发布到测试版渠道，不发布正式版。

## 新增与兼容性变化

- 新增 YouTube 搜索、订阅、关联账号、状态、API Key、OAuth、网页会话和直播刷新接口；所有 YouTube 功能受 `youtube_enabled` 总开关控制，默认关闭。
- YouTube API Key 必须由最终用户在设置页填写并加密保存，服务端不再接受环境变量注入作为主配置路径。OAuth 支持启动、完成、刷新和退出；可导入网页会话 Cookie 以抓取会员限定社区帖。
- 共享核心支持公开社区帖和会员社区帖解析、预约直播与开播/下播、最近直播、场次统计、Super Chat/Super Sticker、会员事件和同步 schema 3。
- 修复关联账号删除事件丢失平台、OAuth 刷新/退出竞态、社区帖来源指纹漂移、会员预览图片泄露和 YouTube 聊天重启后重复计数；聊天消息以 session 级持久 ID 去重。
- 新增 `live_super_chat_events` 与 `live_youtube_chat_events` 表及索引；旧列集或中间测试版列集检测到缺口时，在迁移前使用 `VACUUM INTO` 生成一致性备份。`SERVER_SNAPSHOT_SCHEMA_VERSION` 仍为 1。
- 实时同步协议升级为 schema 3。旧客户端连接服务端时必须提示升级，不得把 schema 3 payload 或缓存交给 schema 2 客户端解析。

## 升级前配置与数据迁移

- 服务端 OAuth 配置为 `OSHICAL_YOUTUBE_CLIENT_ID`、`OSHICAL_YOUTUBE_CLIENT_SECRET`，可选 `OSHICAL_YOUTUBE_OAUTH_REDIRECT_URI`；非回环回调地址必须使用 HTTPS，`localhost`/`127.0.0.1`/`[::1]` 才允许 HTTP。
- 未显式配置 `OSHICAL_SECRETS_KEY` 时，首次启动会生成随机主密钥文件，默认位于数据库同目录的 `<数据库文件名>.secrets-key`，也可用 `OSHICAL_SECRETS_KEY_FILE` 指定数据库外路径。主密钥文件和数据库必须一起备份，Unix 权限为 `0600`，Windows 使用受限 ACL。
- 升级前停止服务，备份数据库及必要的 WAL/SHM、主密钥文件、OAuth 配置和更新代理配置；保留旧镜像、二进制或编译产物。应用内迁移备份是一致性数据库快照，但手工备份仍应包含 WAL/SHM。
- 丢失主密钥文件且没有 `OSHICAL_SECRETS_KEY` 时，旧随机密钥密文无法恢复；必须恢复原密钥文件，或让用户重新填写 YouTube 凭据。回滚到不认识新主密钥文件的旧服务端版本前，务必备份该文件。

## 产物与渠道

- 版本与标签：`0.3.31-beta.1` / `server-v0.3.31-beta.1`。发布适用 GitHub Actions 的 Linux x86_64、Windows x86_64 编译产物，以及 `ghcr.io/yuetc/oshical-server:0.3.31-beta.1` 镜像。
- Release 标记为 prerelease，更新固定 `server-beta` 清单；不移动 `server-latest`、Docker `latest` 或正式版通道。
- 构建、回归、签名、产物验证和发布均由 GitHub Actions 执行。正式版必须在 beta 验收完成后另行确认。

## 已知限制与回滚

1. 社区页当前只解析首屏，不抓取 continuation 多页；预约直播只检查最近 20 个上传；没有统一搜索/频道元数据 TTL 缓存，频繁搜索会消耗用户自己的 API 配额。
2. `streamers.mid` 当前仍为全局唯一，尚未迁移为 `(platform, mid)` 复合唯一约束；跨平台同名 ID 的极端情况仍有限制。
3. 聊天 continuation token 不跨进程持久化，已通过持久消息 ID 去重避免重复累计；聊天事件表暂无独立清理策略，长期使用会增加数据库体积。
4. Super Chat 明细以本地历史为主，跨设备只同步场次级聚合；旧会话升级后互动用户统计可能短暂受旧计数下界影响。
5. 真实 OAuth、Data API、会员 Cookie、SQLite 迁移回滚、Windows ACL 和 B 站完整回归仍需在 beta 周期继续验收；本版不应被视为 stable 已验证版本。
6. 回退时恢复对应数据库、主密钥文件和服务端产物；不要自动把新密文降级回旧的可预测密钥。旧版本可能无法读取新主密钥文件，已迁移的 YouTube 凭据可能需要重新填写。