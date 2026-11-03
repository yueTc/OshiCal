# 服务端权威的多设备同步设计

- 状态：待实现
- 日期：2026-10-29
- 关联模块：crates/core、server、src-tauri、src
- 关联计划：docs/08-client-server-migration-plan.md
- 目标版本：多设备同步版本

## 1. 设计结论

系统改为“服务端权威、客户端只读缓存、离线写操作显式提交”的结构：

1. 服务端是唯一数据源和唯一采集执行者。
2. 客户端不再在服务器模式下抓取 B 站动态、调用日程模型、检查开播或采集直播统计。
3. 客户端本地 SQLite 只保存服务端数据镜像、设备本地设置和离线 mutation。
4. 服务端每次产生正式数据变化时，在同一事务中写入变更日志。
5. 服务端通过 WebSocket 主动推送增量变更批次。
6. 客户端把变更批次和同步游标放在同一个本地事务中应用。
7. 客户端断线后通过变更日志补拉，不依赖 WebSocket 是否一直在线。
8. 如果客户端游标过旧或日志被清理，客户端重新拉服务端快照。
9. 客户端离线手工修改进入 mutation 队列，恢复连接后提交并由服务端做冲突检测。
10. 迁移客户端旧数据到服务端只作为首次接入流程，不作为持续同步机制。

## 2. 为什么不继续采用当前方式

当前方式可以支撑单设备，但不适合多设备可靠同步：

| 当前问题 | 多设备下的后果 |
| --- | --- |
| WebSocket 主要发送 notify 类事件 | 客户端收到事件后整表刷新，流量和延迟随数据量增长 |
| 部分写操作没有事件 | 其他设备可能长时间看不到变化 |
| 事件没有完整实体数据 | 多设备无法直接增量应用，只能再请求全量 |
| WebSocket 历史补发最多 500 条 | 设备离线较久可能漏事件 |
| 没有设备 ID | 无法记录每个设备的游标、在线状态和最后同步位置 |
| 本地数据库同时承担本地模式和服务器缓存 | 切换模式、切换服务器时容易混淆本地数据和服务器数据 |
| 刷新进度绑定单次 HTTP 请求 | 其他设备看不到当前刷新状态 |
| 设置模型混合服务器配置和设备配置 | 一台设备修改通知开关可能影响其他设备 |
| 托盘“立即刷新”直接调用本地 refresh_all | 服务器模式下可能绕开服务器单点采集原则 |
| 删除没有可重放的 tombstone | 离线设备可能保留已删除数据 |

## 3. 目标架构

    B 站动态 / 开播 / 弹幕 / 模型 API
                   |
                   v
    +----------------------------------+
    |           服务端                  |
    |----------------------------------|
    | 单点采集任务                     |
    | 日程解析与合并                   |
    | 服务端 SQLite 权威库             |
    | sync_changes 增量变更日志         |
    | WebSocket 推送                    |
    | 设备注册与游标                     |
    | 冲突检测与 mutation receipt       |
    +----------------------------------+
             |                 |
             | WebSocket       | HTTP
             | 增量批次         | 快照 / 补拉 / 写操作
             v                 v
    +----------------+  +----------------+
    | Windows 设备 A |  | Windows 设备 B |
    | 本地缓存       |  | 本地缓存       |
    | 本地通知       |  | 本地通知       |
    +----------------+  +----------------+

             |
             v
    +----------------+
    | Web / 其他设备 |
    | IndexedDB 或内存 |
    +----------------+

## 4. 角色与职责

### 4.1 服务端职责

- 抓取 B 站动态。
- 解析预约卡片。
- 调用文字或视觉模型。
- 日程合并、取消、来源抑制。
- 定期检查开播状态。
- 生成并维护直播场次。
- 采集直播弹幕、礼物、SC、舰长等公开事件。
- 保存权威数据。
- 生成同步变更。
- 处理多个设备的离线 mutation。
- 推送变更给所有已连接设备。
- 向新设备或失联过久的设备提供完整快照。

### 4.2 客户端职责

- 展示服务端数据。
- 维护本地只读缓存。
- 保存本设备设置。
- 接收同步变更并原子应用。
- 保存同步游标。
- 在断线时允许受控的离线 mutation。
- 在恢复连接后提交 mutation 并处理冲突。
- 根据本设备设置决定是否显示通知。
- 不执行服务端模式下的采集任务。

### 4.3 Web 设备职责

- 在线时直接使用服务端 API 和 WebSocket。
- 可选使用 IndexedDB 保存缓存和游标。
- 不保存 B 站 Cookie 或模型密钥。
- 不提供服务端数据采集入口。

## 5. 数据分类

### 5.1 服务端权威数据

这些数据由服务端创建、修改并同步给所有设备：

- subscriptions
- linked_accounts
- schedules
- schedule_sources
- weekly_schedule_rules
- weekly_schedule_exceptions
- source_suppressions
- streamer live status
- live_sessions
- live_session_stats
- server_settings

### 5.2 设备本地数据

这些数据只属于单个设备，不跨设备同步：

- device_id
- device_name
- server_url
- server_token 引用
- 通知开关
- 开机自启
- 主题
- 侧栏宽度
- 当前页面
- 窗口位置
- 本地日志

### 5.3 临时数据

这些数据通过 WebSocket 临时推送，不要求完整重放：

- refresh job progress
- live online metrics
- heartbeat
- connection status
- transient error

### 5.4 不跨设备数据

- B 站 Cookie。
- 模型 API Key。
- 服务端 Token。
- 本地绝对路径。
- 原始动态缓存。
- 本地模式数据库中的私有数据。

## 6. 设备身份

### 6.1 生成设备 ID

客户端首次启动时生成 UUID：

    device_id = uuid-v4

保存位置：

- Windows 桌面端：系统凭据管理器或本地主数据库。
- Web：IndexedDB 或 localStorage。

device_id 不随服务器迁移改变，也不在设备间共享。

### 6.2 注册设备

新增服务端表：

    CREATE TABLE IF NOT EXISTS devices (
        device_id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        platform TEXT NOT NULL,
        app_version TEXT NOT NULL,
        last_seen_at TEXT NOT NULL,
        last_ack_seq INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        revoked_at TEXT
    );

新增接口：

    POST /api/devices/register

请求：

    {
      "deviceId": "uuid",
      "name": "客厅电脑",
      "platform": "windows",
      "appVersion": "0.2.0"
    }

当前仍采用单用户共享 Bearer Token。设备 ID 主要用于：

- 同步游标。
- 设备在线状态。
- 刷新任务来源。
- 冲突审计。
- 后续按设备撤销 Token 的基础。

### 6.3 设备列表

设置页增加设备列表：

- 设备名称。
- 平台。
- 应用版本。
- 最后在线时间。
- 最后确认的同步序号。
- 当前是否在线。

第一版不强制实现远程下线；后续可为每个设备发独立 Token 并支持撤销。

## 7. 同步协议总体流程

### 7.1 新设备首次连接

    1. 生成或读取 device_id。
    2. 注册设备。
    3. GET /api/sync/bootstrap。
    4. 在本地事务中写入完整快照。
    5. 保存 snapshotRevision 为本地 cursor_seq。
    6. 连接 WebSocket，携带 after=snapshotRevision。
    7. 服务端推送序号大于 cursor_seq 的变更批次。
    8. 客户端原子应用变更并推进 cursor_seq。
    9. 客户端发送 ACK。

### 7.2 已有设备重连

    1. 读取本地 cursor_seq。
    2. 连接 WebSocket，携带 after=cursor_seq。
    3. 服务端检查 cursor 是否仍在保留范围内。
    4. 如果可增量恢复：
       - 推送未消费变更。
    5. 如果 cursor 过旧：
       - 返回 resetRequired。
       - 客户端重新拉快照。
    6. 客户端应用完成后发送 ACK。

### 7.3 其他设备产生写操作

    1. 设备 A 提交 mutation。
    2. 服务端校验 baseRevision。
    3. 服务端在事务中修改实体并写入 sync_changes。
    4. 事务提交后立即广播 sync_batch。
    5. 设备 B、C 按序应用变更。
    6. 设备 A 收到 mutation 结果和对应变更。

### 7.4 服务端后台采集产生写操作

    1. 动态轮询任务发现新日程。
    2. 服务端完成解析、合并和入库。
    3. 每个正式实体变化写入 sync_changes。
    4. 提交后广播 sync_batch。
    5. 所有在线设备增量更新。
    6. 离线设备重连后从 change log 补齐。

### 7.5 删除

- 服务端删除实体时写入 tombstone change。
- tombstone 包含 entityType、entityId、deletedAt、revision。
- 客户端按 tombstone 删除本地缓存。
- 变更日志被快照替代后，快照中不存在的实体即表示已删除。

## 8. 同步变更日志

### 8.1 新表 sync_changes

    CREATE TABLE IF NOT EXISTS sync_changes (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        entity_type TEXT NOT NULL,
        entity_id TEXT NOT NULL,
        operation TEXT NOT NULL,
        entity_revision INTEGER NOT NULL,
        payload_json TEXT,
        actor_type TEXT NOT NULL,
        actor_id TEXT,
        mutation_id TEXT,
        created_at TEXT NOT NULL
    );

索引：

    CREATE INDEX IF NOT EXISTS idx_sync_changes_entity
    ON sync_changes(entity_type, entity_id, seq);

    CREATE INDEX IF NOT EXISTS idx_sync_changes_created
    ON sync_changes(created_at, seq);

字段说明：

| 字段 | 说明 |
| --- | --- |
| seq | 全局单调递增同步序号 |
| entity_type | subscription、link、schedule、weekly_rule 等 |
| entity_id | 服务端实体 ID |
| operation | upsert 或 delete |
| entity_revision | 实体自身版本号 |
| payload_json | upsert 时保存完整实体数据 |
| actor_type | collector、device、system |
| actor_id | 设备 ID 或采集器名称 |
| mutation_id | 客户端操作幂等 ID |

### 8.2 实体版本

为需要并发编辑的实体增加 revision 字段：

- streamers.revision
- linked_accounts.revision
- schedules.revision
- weekly_schedule_rules.revision
- weekly_schedule_exceptions.revision
- source_suppressions.revision
- settings.revision 或独立版本字段

规则：

- 新建实体 revision = 1。
- 每次正式修改 revision = revision + 1。
- 删除时写入 tombstone，不再递增实体。
- 客户端 mutation 必须带 baseRevision。
- 服务端当前 revision 不等于 baseRevision 时返回冲突。

兼容策略：

- 现有 baseUpdatedAt 暂时继续接受。
- 新客户端使用 baseRevision。
- 旧客户端继续按更新时间判断。

### 8.3 写操作必须原子

以下内容必须在同一个 SQLite 事务中完成：

- 正式实体写入。
- sync_changes 写入。
- mutation_receipts 写入。
- 相关级联变化。
- 审计信息。

如果事务失败：

- 实体不变化。
- sync_changes 不出现。
- 其他设备不会收到伪变化。

### 8.4 变更顺序

同一事务内按照依赖顺序生成变更：

1. subscription
2. linked_account
3. weekly_rule
4. weekly_exception
5. schedule
6. schedule_source
7. source_suppression
8. live_status
9. live_session
10. server_settings

客户端必须按 seq 顺序应用。

### 8.5 变更类型

    subscription.upsert
    subscription.delete
    link.upsert
    link.delete
    schedule.upsert
    schedule.delete
    weekly_rule.upsert
    weekly_rule.delete
    weekly_exception.upsert
    weekly_exception.delete
    schedule_source.upsert
    schedule_source.delete
    source_suppression.upsert
    source_suppression.delete
    live_status.upsert
    live_session.upsert
    live_session.delete
    server_settings.upsert

### 8.6 不进入同步日志的数据

- refresh job progress。
- WebSocket heartbeat。
- 临时在线人数。
- 连接状态。
- 本地通知状态。
- 设备本地设置。

这些数据通过临时事件发送，断开后不补发。

## 9. 服务端 API

### 9.1 完整快照

    GET /api/sync/bootstrap

响应：

    {
      "schemaVersion": 2,
      "snapshotRevision": 12345,
      "minRetainedSeq": 8000,
      "serverTime": "2026-10-29T12:00:00Z",
      "serverSettings": {},
      "subscriptions": [],
      "schedules": [],
      "weeklyRules": [],
      "weeklyExceptions": [],
      "scheduleSources": [],
      "sourceSuppressions": [],
      "liveStatuses": [],
      "liveSessions": []
    }

要求：

- 在一次 SQLite 读事务中读取。
- snapshotRevision 在读取开始时确定。
- 客户端应用快照后立即以 snapshotRevision 作为 cursor。
- 快照接口支持 gzip。
- 旧 /api/sync/snapshot 暂时保留，供旧客户端使用。

### 9.2 增量变更

    GET /api/sync/changes?after=12345&limit=200

响应：

    {
      "schemaVersion": 2,
      "fromSeq": 12346,
      "toSeq": 12545,
      "latestSeq": 12600,
      "minRetainedSeq": 8000,
      "hasMore": true,
      "resetRequired": false,
      "changes": [
        {
          "seq": 12346,
          "entityType": "schedule",
          "entityId": "456",
          "operation": "upsert",
          "entityRevision": 3,
          "payload": {}
        }
      ]
    }

规则：

- limit 默认 200，最大 500。
- after 小于 minRetainedSeq - 1 时返回 resetRequired。
- 客户端必须按 seq 顺序应用。
- 返回完整当前实体，不需要客户端再请求详情。

### 9.3 同步确认

    POST /api/sync/ack

请求：

    {
      "deviceId": "uuid",
      "seq": 12545
    }

用途：

- 记录设备最后同步位置。
- 设备列表展示。
- 后续可用于变更日志清理策略。

### 9.4 离线 mutation

继续使用：

    POST /api/sync/mutations

新增要求：

- 每个 mutation 带 baseRevision。
- 每个 mutation 带 deviceId。
- 服务端在事务中应用实体和 sync_changes。
- 冲突返回服务器当前实体和 revision。
- 同一 mutationId 幂等。

### 9.5 刷新任务

多设备环境下，手动刷新改为异步任务：

    POST /api/jobs/refresh
    POST /api/jobs/refresh/{streamerId}

响应：

    {
      "jobId": "uuid",
      "status": "queued"
    }

服务端要求：

- 同一类型刷新任务合并或排队。
- 同一主播不能并发刷两次。
- 所有设备都能收到进度。
- 任务完成后只通过 sync_changes 推送正式数据变化。

### 9.6 设备接口

    POST /api/devices/register
    GET /api/devices
    POST /api/devices/heartbeat

第一版可只实现 register 和 list。

## 10. WebSocket 协议

### 10.1 连接参数

    GET /api/ws?after=<cursor>&protocol=2

### 10.2 鉴权

客户端第一帧：

    {
      "type": "auth",
      "token": "server-token",
      "deviceId": "uuid",
      "protocolVersion": 2
    }

### 10.3 鉴权响应

    {
      "type": "auth_ok",
      "protocolVersion": 2,
      "deviceId": "uuid",
      "latestSeq": 12600,
      "minRetainedSeq": 8000,
      "resetRequired": false
    }

### 10.4 增量批次

    {
      "type": "sync_batch",
      "fromSeq": 12346,
      "toSeq": 12445,
      "latestSeq": 12600,
      "changes": []
    }

批次限制：

- 每批最多 100 个变更。
- 每批建议不超过 512 KiB。
- 超大批次拆分为多个 sync_batch。
- 服务端可发送空批次 sync_available，提示客户端主动补拉。

### 10.5 确认

客户端成功写入本地事务后：

    {
      "type": "ack",
      "seq": 12445
    }

### 10.6 需要重置

    {
      "type": "reset_required",
      "reason": "cursor_expired",
      "latestSeq": 12600,
      "minRetainedSeq": 8000
    }

客户端收到后：

1. 停止应用增量。
2. 拉取 /api/sync/bootstrap。
3. 事务替换本地缓存。
4. 保存 snapshotRevision。
5. 用新的 after 重连。

### 10.7 临时任务事件

    {
      "type": "job_progress",
      "jobId": "uuid",
      "kind": "refresh_streamer",
      "streamerId": 123,
      "stage": "识别图片日程",
      "completed": 2,
      "total": 5,
      "percent": 60,
      "done": false,
      "failed": false
    }

临时任务事件不进入 sync_changes。

### 10.8 心跳

- 服务端每 30 秒发送 Ping。
- 客户端必须回应 Pong。
- 客户端也可每 30 秒发送应用层 heartbeat。
- 超过 90 秒无响应视为离线。

## 11. 服务端推送策略

### 11.1 持久化变化

以下变化必须持久化到 sync_changes 并推送：

- 订阅新增、修改、删除。
- 官号关联新增、删除。
- 日程新增、修改、合并、取消、删除。
- 固定周表新增、修改、删除。
- 周表例外新增、删除。
- 来源抑制新增、删除。
- 开播状态发生变化。
- 直播场次创建、更新、结束。
- 服务端设置修改。

### 11.2 即时推送

事务提交后：

1. 运行时向 broadcast 通道发布 sync_batch。
2. 客户端立即应用。
3. 如果当时没有客户端或广播丢失，change log 仍然完整。
4. 后台同步泵每 500ms 检查一次新 change，作为兜底。

### 11.3 不依赖定时轮询推送

服务端采集任务仍按周期运行：

- 开播检查默认 20 秒。
- 动态刷新默认 300 秒。

但只有正式数据产生变化时才生成同步变更。

### 11.4 临时数据

以下数据可在不改变正式实体时临时推送：

- 当前在线人数。
- 当前直播标题变化即使还未结束场次。
- 刷新任务进度。
- 连接状态。

如果客户端错过临时数据，可通过 REST 拉取当前状态。

## 12. 客户端缓存设计

### 12.1 数据隔离

当前桌面数据库同时保存本地模式和服务器导入数据，不适合多设备长期运行。

建议改为：

    app.db
      - 本地模式权威数据
      - 设备设置
      - 服务器列表
      - 离线队列索引

    servers/<server-hash>/cache.db
      - 服务端镜像
      - sync_changes 应用结果
      - cursor_seq
      - last_snapshot_revision
      - last_ack_seq

好处：

- 本地模式数据不被服务器数据覆盖。
- 切换不同服务器不混淆 ID。
- 服务器缓存可以安全重建。
- 多设备游标独立。

### 12.2 本地同步元数据

    CREATE TABLE IF NOT EXISTS sync_meta (
        server_key TEXT PRIMARY KEY,
        device_id TEXT NOT NULL,
        cursor_seq INTEGER NOT NULL DEFAULT 0,
        last_snapshot_revision INTEGER NOT NULL DEFAULT 0,
        last_ack_seq INTEGER NOT NULL DEFAULT 0,
        last_sync_at TEXT,
        status TEXT NOT NULL DEFAULT 'idle'
    );

### 12.3 应用批次

客户端收到 sync_batch 后：

1. 校验 fromSeq 是否等于本地 cursor_seq + 1。
2. 如果存在缺口，不应用，先走 HTTP 补拉。
3. 开启本地事务。
4. 按 seq 顺序 upsert 或 delete。
5. 更新 cursor_seq。
6. 提交事务。
7. 发送 ACK。

如果第 4 或第 5 步失败：

- 事务回滚。
- cursor_seq 不变。
- 可重新拉取同一批。

### 12.4 快照替换

收到 resetRequired 或首次连接时：

1. 拉取完整快照。
2. 在本地事务中清空服务器缓存表。
3. 写入快照数据。
4. 写入 snapshotRevision。
5. 提交。
6. 发送 ACK。
7. 用 snapshotRevision 重连。

### 12.5 本地模式和服务器模式

- 服务器模式读写 servers/<hash>/cache.db。
- 本地模式读写 app.db。
- 切换模式只切换数据源，不复用实体 ID。
- 服务器模式下客户端不得调用本地 B 站或模型接口。

## 13. 服务端采集任务设计

### 13.1 单点执行

服务端启动时只启动一套：

- live monitor worker。
- dynamic refresh worker。
- event/sync publish worker。
- live stats manager。

设备连接不得启动新的采集循环。

### 13.2 刷新任务协调

增加 RefreshJobManager：

    job_id
    kind
    streamer_id
    requested_by_device
    status
    progress
    started_at
    finished_at
    report

规则：

- 同一主播已有运行时，新请求加入已有任务，不重复执行。
- 全量刷新和单主播刷新分开。
- 后台轮询与手动刷新共享同一锁。
- 所有设备都能看到进度。
- 任务完成后广播 job_finished。

### 13.3 托盘刷新

桌面端托盘“立即刷新”在服务器模式下必须调用：

    POST /api/jobs/refresh

不得调用本地 AppService::refresh_all。

本地模式保持现状。

### 13.4 采集结果

采集器只写服务端数据库，不同步：

- 原始 B 站动态。
- 原始图片。
- 模型原始响应。
- 弹幕原始事件。
- 礼物原始事件。

客户端只接收：

- 结构化日程。
- 主播状态。
- 直播场次汇总。
- 服务端设置。

## 14. 设置拆分

### 14.1 服务端设置

由服务端所有者管理并同步到所有设备：

- live_poll_seconds
- dynamic_poll_seconds
- auto_analyze_images
- live_stats_enabled
- llm_base_url
- llm_model
- llm_prompt_version
- proxy_port

### 14.2 设备设置

只保存在设备本地：

- data_mode
- server_url
- server_token
- notifications_enabled
- autostart
- theme
- sidebar_width
- current_page
- window_position

### 14.3 规则

- 设备 A 打开通知，不影响设备 B。
- 服务端轮询间隔由一台设备修改后，所有设备读同一值。
- 服务端设置通过 sync_changes 推送。
- 设备设置不进入服务端数据库。
- Cookie 和 API Key 仍然只来自服务端环境变量。

## 15. 冲突处理与离线写

### 15.1 mutation 模型

    {
      "mutationId": "uuid",
      "deviceId": "uuid",
      "entity": "schedule",
      "operation": "update",
      "baseRevision": 4,
      "payload": {},
      "createdAt": "..."
    }

### 15.2 服务端处理

1. 查询 mutation_receipts。
2. 如果同一 mutationId 已应用，返回原结果。
3. 读取实体当前 revision。
4. 如果 baseRevision 不匹配，返回 conflict。
5. 校验业务规则。
6. 在一个事务中写入实体、mutation_receipt 和 sync_changes。
7. 提交。
8. 广播变更。

### 15.3 冲突响应

    {
      "status": "conflict",
      "entityType": "schedule",
      "entityId": 456,
      "baseRevision": 4,
      "serverRevision": 6,
      "server": {},
      "client": {}
    }

设备端允许选择：

- 采用服务器版本。
- 保留客户端版本并重新提交。
- 手工合并。

不得静默覆盖。

### 15.4 手工锁定

- 任意设备修改日程后，服务端设置 manual_locked。
- 其他设备通过 sync_changes 收到 manual_locked。
- 自动采集不能覆盖。
- 删除自动日程写入来源抑制。
- 删除同步为 tombstone。

## 16. 首次接入与数据迁移

### 16.1 服务端状态

服务端增加初始化状态：

    uninitialized
    active

判断依据：

- 没有订阅、日程、周表。
- 没有任何已注册设备完成初始化。

### 16.2 第一台设备

第一台设备首次连接时可以选择：

- 使用服务端数据。
- 迁移本机数据到服务端。

迁移流程见 docs/08-client-server-migration-plan.md。

### 16.3 后续设备

后续设备默认只提供：

- 使用服务端数据。
- 取消。

不再默认提供“迁移本机数据”，避免多个设备互相覆盖。

如需从某台设备再次导入，设置页提供显式管理操作。

### 16.4 迁移后的同步

迁移完成后：

1. 服务端生成完整 sync_changes。
2. 客户端切换到独立服务器缓存。
3. 客户端以迁移后的 snapshotRevision 建立 cursor。
4. 后续不再使用一次性 import_snapshot 作为日常同步。

## 17. 通知策略

- 每个设备独立决定是否显示通知。
- 服务端推送 live_status 变化。
- 客户端只在本地观察到未开播到开播时通知。
- 重放批次不通知。
- 启动快照不通知。
- 同一场直播使用 streamerId + startedAt 作为本地通知去重键。
- 设备 A 显示通知不代表设备 B 也必须显示。
- 关闭通知不修改服务端数据。

## 18. 数据库变更

### 18.1 服务端新增表

- sync_changes
- devices
- device_sync_state
- migration_receipts（由 08 定义）

### 18.2 服务端新增列

- 业务实体 revision。
- sync_queue.device_id。
- sync_queue.server_key。

### 18.3 客户端新增表

- sync_meta
- server_registry
- device_identity
- outbox_queue 或带 server_key 的 sync_queue

### 18.4 兼容

- 新表只增不删。
- revision 使用默认值 0，启动后回填为 1。
- 旧客户端继续使用 /api/sync/snapshot 和旧事件。
- 新客户端使用 protocolVersion 2。
- 新服务端同时支持两种协议一段时间。

## 19. 性能与容量目标

第一版目标：

- 支持至少 10 台在线设备。
- 单设备同步变更延迟 P95 小于 1 秒。
- 跨公网 P95 小于 3 秒，具体取决于网络。
- 单批最多 100 个变更。
- 单批最多 512 KiB。
- 单次 HTTP 补拉最多 500 个变更。
- sync_changes 保留至少 30 天或 100000 条。
- 超限客户端通过快照恢复。
- 快照接口支持 gzip。
- 所有客户端游标和变更应用必须可重试。

## 20. 安全要求

- 所有同步接口必须鉴权。
- WebSocket 首帧必须鉴权。
- 服务器非回环绑定必须配置 Token。
- 公网必须 HTTPS/WSS。
- 设备 ID 不替代 Token。
- 迁移接口不接收 Cookie、API Key。
- 变更 payload 不包含敏感配置。
- 设备列表不显示 Token。
- 日志不得打印完整变更 payload。
- 设备注册必须经过 Token 鉴权。
- 后续可增加每设备 Token 和撤销能力。

## 21. 可观测性

服务端指标：

- 当前在线设备数。
- 每设备最后 ACK 序号。
- 最新 sync seq。
- 未确认变更数量。
- 每批推送大小。
- 推送失败次数。
- 需要快照重置次数。
- 刷新任务耗时。
- 动态抓取成功和失败次数。

客户端状态：

- 当前 cursor_seq。
- 最新服务端 seq。
- 连接状态。
- 最后同步时间。
- 是否存在缺口。
- 离线 mutation 数量。
- 冲突数量。

## 22. 测试计划

### 22.1 服务端单元测试

- 每次正式写入都产生连续 sync seq。
- 事务失败时不产生 sync_changes。
- revision 按规则递增。
- tombstone 正确生成。
- after 过旧时返回 resetRequired。
- ACK 更新设备游标。
- 同一 mutationId 幂等。
- 冲突返回服务器当前实体。

### 22.2 WebSocket 测试

- 两台设备同时连接并收到相同变更。
- 批次顺序连续。
- 断线重连只补未消费变更。
- 游标过期触发 resetRequired。
- 广播积压触发 HTTP 补拉。
- heartbeat 超时清理连接。
- 非重放事件不触发通知。

### 22.3 客户端测试

- 快照与游标在同一事务写入。
- sync_batch 原子应用。
- 中途失败 cursor 不前进。
- 缺口时不跳序应用。
- 缓存重建后 cursor 正确。
- 设备设置不写服务器。
- 服务器模式不启动本地采集。
- 托盘刷新走服务器接口。

### 22.4 多设备端到端

1. 设备 A 手工新增日程。
   - 设备 B 1 秒内看到。
2. 服务端自动刷新产生日程。
   - 所有在线设备看到。
3. 设备 A 断网修改日程。
   - 恢复后提交成功。
   - 设备 B 收到最终变更。
4. 设备 B 离线超过保留窗口。
   - 重连后收到 resetRequired。
   - 拉快照恢复。
5. 设备 A 删除日程。
   - 设备 B 删除对应缓存。
6. 设备 A 修改通知设置。
   - 设备 B 不受影响。
7. 设备 A 修改服务端轮询间隔。
   - 设备 B 收到 server_settings 变更。
8. 两台设备同时修改同一日程。
   - 后者收到冲突，不静默覆盖。
9. 设备 A 接入新服务器并迁移本地数据。
   - 设备 B 随后加入并联通。

## 23. 实施阶段

### Phase 0：协议与模型

- [ ] 确定 protocolVersion 2。
- [ ] 确定 sync_changes 结构。
- [ ] 确定 revision 规则。
- [ ] 确定设备本地设置与服务端设置边界。
- [ ] 确定变更日志保留策略。

### Phase 1：服务端权威变更日志

- [ ] 新增 sync_changes。
- [ ] 为业务实体增加 revision。
- [ ] 抽象带变更日志的事务写入。
- [ ] 覆盖订阅、官号、日程、周表、直播状态和设置。
- [ ] 增加连续性和幂等测试。

### Phase 2：同步 API

- [ ] 新增 /api/sync/bootstrap。
- [ ] 新增 /api/sync/changes。
- [ ] 新增 /api/sync/ack。
- [ ] 扩展 mutation 的 baseRevision 和 deviceId。
- [ ] 保留旧快照接口兼容。

### Phase 3：WebSocket v2

- [ ] 支持 deviceId 和 protocolVersion。
- [ ] 实现 sync_batch。
- [ ] 实现 ACK。
- [ ] 实现 reset_required。
- [ ] 实现补拉和广播积压处理。
- [ ] 实现临时 job_progress 通道。

### Phase 4：服务端任务协调

- [ ] 新增 RefreshJobManager。
- [ ] 手动刷新改为异步任务。
- [ ] 合并重复刷新请求。
- [ ] 所有设备广播任务进度。
- [ ] 托盘刷新路由到服务器。
- [ ] 保证服务器模式不运行本地采集。

### Phase 5：客户端服务器缓存

- [ ] 拆分 app.db 和 per-server cache.db。
- [ ] 新增 sync_meta。
- [ ] 实现快照应用。
- [ ] 实现 sync_batch 原子应用。
- [ ] 实现缺口补拉和 reset。
- [ ] 实现设备 ID。
- [ ] 实现设备设置隔离。

### Phase 6：离线 mutation

- [ ] sync_queue 关联 server_key。
- [ ] mutation 使用 baseRevision。
- [ ] 提交后更新本地 revision。
- [ ] 实现冲突 UI。
- [ ] 实现 mutation receipt。

### Phase 7：首次接入和迁移

- [ ] 第一台设备双路径 onboarding。
- [ ] 后续设备默认只拉服务器。
- [ ] 迁移完成后落到独立服务器缓存。
- [ ] 实现设备列表。
- [ ] 实现重新同步入口。

### Phase 8：验证与发布

- [ ] 10 台设备模拟连接。
- [ ] 断网、重连、快照恢复。
- [ ] 冲突处理。
- [ ] 设置隔离。
- [ ] 通知去重。
- [ ] Docker 部署验证。
- [ ] 更新 README、验收和已知问题。

## 24. 粗略工作量

| 阶段 | 预计 |
| --- | --- |
| Phase 0 | 1 天 |
| Phase 1 | 3 天 |
| Phase 2 | 2 天 |
| Phase 3 | 2 天 |
| Phase 4 | 2 天 |
| Phase 5 | 4 天 |
| Phase 6 | 2 天 |
| Phase 7 | 2 天 |
| Phase 8 | 2 天 |
| 合计 | 约 20 个工程日 |

该估算不包含多用户权限系统、正式公网压测和每设备独立 Token。

## 25. 验收标准

1. 服务器模式下，只有服务端执行采集和模型调用。
2. 两台以上设备同时连接时，任意正式数据变化都能同步。
3. 服务端自动抓取的新日程无需客户端主动刷新即可到达设备。
4. 客户端收到的是结构化增量数据，不需要每次收到事件后整表刷新。
5. WebSocket 断开后，客户端可按 cursor 补拉。
6. 游标过旧时，客户端能自动快照恢复。
7. 删除操作有 tombstone 并能同步到其他设备。
8. 同一变更不会重复应用。
9. 客户端本地事务失败时 cursor 不前进。
10. 设备离线写操作可提交并检测冲突。
11. 通知开关是设备本地设置，不跨设备同步。
12. 服务端轮询和模型设置跨设备同步。
13. 第一台设备可迁移本地数据到服务端。
14. 后续设备默认使用服务端数据。
15. 托盘刷新在服务器模式下走服务端任务接口。
16. 服务端支持至少 10 台在线设备。
17. Core、Server API、Tauri、前端和多设备端到端测试通过。

## 26. 推荐默认决策

- 服务端权威，客户端缓存可随时重建。
- WebSocket 推送结构化增量，HTTP change log 作为可靠补拉。
- 快照和变更日志并存，不用无限增长的事件历史支撑同步。
- Windows 桌面端为每个服务器使用独立缓存数据库。
- mutation 使用 baseRevision，不使用静默 last-write-wins。
- 通知、窗口、主题和自启属于设备本地设置。
- 采集、模型、轮询和代理属于服务端设置。
- 第一台设备可迁移，后续设备只加入。
- 变更日志保留 30 天或 100000 条，超出后回收并允许快照重建。
- 单用户共享 Token 作为第一版安全模型，后续再增加设备级 Token。
