# 客户端 ↔ 服务端首次连接与数据迁移实现计划

- 状态：待实现
- 日期：2026-10-29
- 关联模块：crates/core、server、src-tauri、src
- 说明：客户端到服务端迁移仅作为首次接入子流程；多设备同步架构以 docs/09-server-authoritative-multi-device-sync.md 为准。
- 目标版本：下一功能版本

## 1. 目标

首次连接一台新的服务端时，桌面客户端必须明确提供两条路径：

1. **使用服务端数据**
   - 拉取服务端快照。
   - 与服务端冲突时，由用户选择“服务器覆盖客户端”“保留客户端”或“只导入不冲突项”。
   - 该流程只修改客户端本地缓存，不修改服务端业务数据。
2. **把本机数据迁移到服务端**
   - 把客户端本地订阅、官号、日程和固定周表上传到服务端。
   - 上传前由服务端生成冲突预览。
   - 用户确认冲突策略后，服务端在一个事务中完成导入。
   - 导入完成后，客户端重新拉取服务端快照并重建本地 ID 映射。

核心原则：

- 不允许静默覆盖服务端数据。
- 不允许在用户选择前自动写入服务端业务数据。
- 迁移失败必须整体回滚，不能留下半套订阅或日程。
- 同一次迁移请求必须可安全重试。
- 不迁移 Cookie、Token、API Key 等敏感信息。

## 2. 当前状态与缺口

当前实现只有单向的 **服务端 → 客户端** 导入：

- 客户端连接服务器后调用 GET /api/sync/snapshot。
- Tauri 命令 preview_server_snapshot 只读取客户端数据库并比较冲突。
- Tauri 命令 import_server_snapshot 明确只修改客户端数据库。
- 当前“服务器覆盖客户端”“保留客户端”“只导入不冲突项”都是客户端侧的冲突处理策略。
- 服务端没有接收客户端快照的接口。
- 现有 /api/sync/mutations 只适合小规模离线操作补交，不是全量迁移接口。

相关代码：

- src/App.tsx:593
- src-tauri/src/lib.rs:1137
- crates/core/src/store/import.rs:176
- server/src/api.rs:448
- server/src/api.rs:474

本计划新增：

- 客户端快照导出。
- 服务端迁移预览。
- 服务端迁移应用。
- 首次连接向导。
- 迁移幂等、冲突处理和迁移后 ID 重建。

## 3. 迁移范围

### 3.1 本次迁移的数据

| 数据 | 是否迁移 | 说明 |
| --- | --- | --- |
| 订阅主播 | 是 | 按 MID 去重 |
| 关联官号 | 是 | 按“主播 MID + 官号 MID”去重 |
| 日程 | 是 | 保留参与者、来源、手工锁定、取消状态 |
| 固定周表 | 是 | 按 rule_key 去重 |
| 周表例外 | 是 | 保留 override / skip，避免已改期或已删除场次复活 |
| 日程多来源 | 是 | 保留团播及多来源合并关系 |
| 来源抑制记录 | 是 | 保留用户删除自动日程后的抑制语义 |
| 非敏感设置 | 单独确认 | 不放进数据快照，避免与数据迁移混淆 |
| B 站 Cookie | 否 | 只来自服务端环境变量 |
| 模型 API Key | 否 | 只来自服务端环境变量 |
| 服务端 Token | 否 | 仅用于鉴权，不进入快照 |
| 动态原文与分析缓存 | 否 | 可重新抓取或重新分析 |
| 直播状态、直播场次、直播统计 | 否 | 属于服务端运行时数据 |
| 事件日志 | 否 | 迁移完成后由服务端生成新事件 |
| remote_entity_map | 否 | 迁移后重新建立 |
| sync_queue | 否 | 迁移前必须为空 |

### 3.2 明确不做

- 不做持续双向数据库同步。
- 不做多用户数据隔离。
- 不做多服务端配置同时在线。
- 不迁移历史直播统计。
- 不在迁移接口中接收 Cookie、API Key 或任意环境变量。

## 4. 产品流程

### 4.1 首次连接或更换服务器

桌面端选择服务器模式后：

1. 测试服务器连通性和 Token。
2. 如果当前服务器 URL 已有成功迁移记录，直接按普通重连处理。
3. 如果是首次连接，显示“服务端数据初始化”向导：

    这台服务器还没有初始化客户端数据，请选择：

    [ 使用服务端数据 ]
      用服务器现有订阅和日程初始化本机缓存。

    [ 迁移本机数据到服务端 ]
      把本机订阅、日程和固定周表上传到服务器。

    [ 取消 ]
      保持本地模式。

4. 选择“使用服务端数据”后，继续显示现有导入冲突面板。
5. 选择“迁移本机数据到服务端”后，进入服务端迁移预览。
6. 迁移未完成前，不切换最终数据模式，不启动服务器模式下的写入。

### 4.2 使用服务端数据

该流程复用现有“服务端 → 客户端”导入：

1. 拉取 /api/sync/snapshot。
2. 本地生成冲突预览。
3. 用户选择客户端侧策略。
4. 写客户端数据库。
5. 启动远程事件监听。

该流程不得调用服务端迁移接口。

### 4.3 迁移客户端到服务端

1. 检查本地 sync_queue。
   - 如果存在待同步或冲突 mutation，停止迁移并提示先处理。
2. 从本机 SQLite 生成客户端快照。
3. 调用 POST /api/migration/preview。
4. 显示：
   - 新增数量。
   - 冲突数量。
   - 服务端独有数量。
   - 按实体分类的冲突摘要。
5. 用户选择：
   - **客户端覆盖服务端冲突项**。
   - **保留服务端冲突项，只导入缺失项**。
   - **取消迁移**。
6. 如果采用客户端覆盖，且会覆盖服务端手工锁定日程，需要额外二次确认。
7. 调用 POST /api/migration/apply。
8. 服务端事务提交后，客户端重新拉取 /api/sync/snapshot。
9. 使用现有本地导入逻辑以“服务器数据”策略刷新本地镜像和 remote_entity_map。
10. 启动远程事件监听，进入服务器模式。

### 4.4 普通重连

以下情况不重复显示初始化向导：

- 同一客户端实例。
- 同一服务器 URL。
- 已成功完成“使用服务端数据”或“迁移客户端到服务端”。

记录键建议：

    onboarding_completed:<client_id>:<server_key>

设置页保留“重新导入或迁移数据”入口，供用户主动再次执行。

## 5. 快照模型

### 5.1 通用数据快照

新增通用快照结构，服务端快照和客户端快照共享业务数据字段：

    pub struct DataSnapshot {
        pub schema_version: u32,
        pub exported_at: DateTime<Utc>,
        pub subscriptions: Vec<Streamer>,
        pub schedules: Vec<Schedule>,
        pub weekly_rules: Vec<WeeklyScheduleRule>,
        pub weekly_exceptions: Vec<WeeklyScheduleException>,
        pub schedule_sources: Vec<ScheduleSource>,
        pub source_suppressions: Vec<SourceSuppression>,
    }

服务端快照在通用结构之外继续包含：

    pub struct ServerSnapshot {
        pub data: DataSnapshot,
        pub latest_event_seq: i64,
    }

兼容策略：

- 新字段全部使用 serde default。
- 读取旧快照时，缺失的例外、多来源和抑制记录按空数组处理。
- 客户端快照的 schema_version 从 1 开始。
- 服务端必须拒绝高于当前支持版本的快照。
- 原有 /api/sync/snapshot 的 JSON 保持兼容，可暂时继续平铺字段，再逐步迁移到 data 结构。

### 5.2 快照大小限制

沿用并补充限制：

- 订阅最多 1000。
- 日程最多 20000。
- 固定周表最多 5000。
- 单条标题最多 200 字。
- 单条参与者名称最多 80 字，最多 20 人。
- 头像和图片 URL 必须限制长度。
- 请求体上限建议 32 MiB。
- 超过限制返回 MIGRATION_TOO_LARGE。

### 5.3 快照只读约束

客户端快照必须排除：

- server_token
- bili_cookie
- llm_api_key
- 任意环境变量
- 动态原文和分析缓存
- 本机绝对路径

## 6. 冲突识别规则

| 实体 | 匹配键 |
| --- | --- |
| 订阅 | 主播 MID |
| 官号 | 主播 MID + 官号 MID |
| 日程 | 主播 MID + source_identity |
| 无 source_identity 的日程 | 主播 MID + 开始时间 + 标题 |
| 固定周表 | rule_key |
| 周表例外 | 固定周表 rule_key + occurrence_date |
| 日程多来源 | 日程匹配键 + source_identity |
| 来源抑制 | 主播 MID + source_post_id + source_fingerprint |

冲突策略：

### client

- 冲突订阅：客户端字段覆盖服务端。
- 冲突官号：客户端字段覆盖服务端。
- 冲突日程：客户端整体覆盖服务端。
- 冲突周表：客户端整体覆盖服务端。
- 冲突例外和多来源：按客户端记录覆盖或补齐。

### server

- 冲突项保留服务端。
- 客户端只插入服务端不存在的实体。
- 返回 skippedConflicts。

冲突记录至少包含：

    {
      "entity": "schedule",
      "key": "mid:123/source_identity:text:456",
      "label": "某场直播",
      "serverId": 12,
      "clientId": 99,
      "server": {},
      "client": {},
      "requiresExtraConfirmation": false
    }

冲突明细最多返回 500 条，计数保持完整；超出部分标记 truncated: true。

## 7. 服务端 API

### 7.1 迁移预览

    POST /api/migration/preview
    Authorization: Bearer <token>
    Content-Type: application/json

请求：

    {
      "migrationId": "uuid",
      "clientId": "uuid",
      "snapshot": {
        "schemaVersion": 1,
        "exportedAt": "2026-10-29T00:00:00Z",
        "subscriptions": [],
        "schedules": [],
        "weeklyRules": [],
        "weeklyExceptions": [],
        "scheduleSources": [],
        "sourceSuppressions": []
      }
    }

响应：

    {
      "ok": true,
      "data": {
        "counts": {
          "subscriptions": 10,
          "schedules": 100,
          "weeklyRules": 4
        },
        "conflicts": [],
        "conflictCount": 3,
        "truncated": false,
        "serverStateDigest": "sha256:..."
      }
    }

要求：

- 只读，不写数据库。
- 校验完整引用关系。
- serverStateDigest 用于检测预览后服务端数据是否变化。
- 同一 migrationId 可用于预览，但不能把预览结果当成提交凭据。

### 7.2 应用迁移

    POST /api/migration/apply
    Authorization: Bearer <token>
    Content-Type: application/json

请求：

    {
      "migrationId": "uuid",
      "clientId": "uuid",
      "strategy": "client",
      "expectedServerDigest": "sha256:...",
      "snapshot": {
        "schemaVersion": 1,
        "exportedAt": "2026-10-29T00:00:00Z",
        "subscriptions": [],
        "schedules": [],
        "weeklyRules": [],
        "weeklyExceptions": [],
        "scheduleSources": [],
        "sourceSuppressions": []
      }
    }

响应：

    {
      "ok": true,
      "data": {
        "migrationId": "uuid",
        "subscriptions": 10,
        "linkedAccounts": 2,
        "schedules": 100,
        "weeklyRules": 4,
        "skippedConflicts": 0,
        "created": 95,
        "updated": 5,
        "serverStateDigest": "sha256:..."
      }
    }

错误码：

| 错误码 | 含义 |
| --- | --- |
| MIGRATION_INVALID_SNAPSHOT | 快照结构或引用关系无效 |
| MIGRATION_TOO_LARGE | 超过数据或请求体限制 |
| MIGRATION_STALE_PREVIEW | 预览后服务端数据变化 |
| MIGRATION_ID_REUSED | 同一 migrationId 用于不同请求 |
| MIGRATION_QUEUE_NOT_EMPTY | 客户端存在未处理离线队列 |
| MIGRATION_BACKUP_FAILED | 预迁移备份失败 |
| MIGRATION_CONFLICT_CONFIRM_REQUIRED | 需要覆盖手工锁定日程但未确认 |

### 7.3 请求体限制

Axum 对 JSON 默认请求体有限制。迁移路由必须单独配置：

- 推荐 32 MiB。
- 只对 /api/migration/* 放开。
- 其他 API 继续保持较小限制。
- 客户端应在发送前预检查并给出明确错误。

## 8. 服务端导入算法

应用迁移使用单个 BEGIN IMMEDIATE 事务。

处理顺序：

1. 校验 migrationId、clientId、strategy 和 snapshot。
2. 查询 migration_receipts。
   - 如果已存在且请求摘要一致，直接返回原结果。
   - 如果已存在但请求摘要不同，返回 MIGRATION_ID_REUSED。
3. 计算服务端当前相关数据摘要。
4. 比较 expectedServerDigest。
   - 不一致时返回 MIGRATION_STALE_PREVIEW。
5. 建立客户端 streamer_id 到服务端 streamer_id 的映射：
   - 按 MID 查找已有订阅。
   - 缺失则创建。
   - 冲突按 strategy 处理。
6. 导入官号：
   - 根据映射后的 streamer ID 写入。
   - 按 streamer_id 和 mid 去重。
7. 导入固定周表：
   - 按 rule_key 查找。
   - 建立客户端 rule ID 到服务端 rule ID 的映射。
8. 导入日程：
   - 将快照中的客户端 streamer ID 和 rule ID 替换为服务端 ID。
   - 保留 manual_locked、cancelled、来源字段和参与者。
   - 按 source_identity 或开始时间加标题匹配。
9. 导入日程多来源和来源抑制记录。
10. 导入周表例外。
11. 写入迁移事件：
    - 建议新增 data_imported 事件。
    - 前端收到后调用 refreshActiveData。
12. 写入 migration_receipts：
    - migration_id
    - request_digest
    - result_json
    - client_id
    - applied_at
13. 提交事务。

任何一步失败都回滚整个事务。

## 9. 数据库变更

新增表：

    CREATE TABLE IF NOT EXISTS migration_receipts (
        migration_id TEXT PRIMARY KEY,
        client_id TEXT NOT NULL,
        request_digest TEXT NOT NULL,
        result_json TEXT NOT NULL,
        applied_at TEXT NOT NULL
    );

可选增加索引：

    CREATE INDEX IF NOT EXISTS idx_migration_receipts_client
    ON migration_receipts(client_id, applied_at);

兼容要求：

- 只做新增表，不重建现有表。
- 启动时由 SqliteStore::initialize 幂等创建。
- 旧数据库无需手工迁移。
- 回滚旧程序时，多余表不影响现有逻辑。

## 10. 核心代码改造

### 10.1 crates/core/src/models.rs

- 新增 DataSnapshot。
- 新增 WeeklyScheduleException、ScheduleSource 的公开模型。
- 新增迁移预览、冲突、报告结构。
- 保持旧 ServerSnapshot 兼容。

### 10.2 crates/core/src/store/import.rs

- 抽取服务端和客户端共用的快照校验。
- 保留现有 preview_snapshot 和 import_snapshot 用于服务端到客户端。
- 新增服务端侧迁移预览函数。
- 新增服务端侧 apply_client_migration。
- 允许注入当前时间，方便测试。
- 统一冲突键生成函数，避免预览和提交使用不同规则。

### 10.3 crates/core/src/store.rs

- 初始化 migration_receipts。
- 增加查询、写入迁移回执函数。
- 增加读取快照扩展数据的函数：
  - 周表例外。
  - 日程多来源。
  - 来源抑制。
- 增加服务端相关数据摘要计算。

### 10.4 server/src/api.rs

- 新增 /api/migration/preview。
- 新增 /api/migration/apply。
- 迁移路由挂入现有鉴权中间件。
- 单独配置请求体上限。
- 将 AppError 映射为对应 HTTP 错误。
- 应用成功后写入 data_imported 事件。

### 10.5 server/src/runtime.rs 和 server/src/config.rs

- 如果启用自动预迁移备份，传入数据库路径或备份目录。
- 建议在导入前执行 VACUUM INTO。
- 备份文件名示例：

    oshical-before-migration-<migration_id>.db

- 备份已存在时拒绝覆盖。
- 备份失败则中止迁移。

### 10.6 src-tauri/src/lib.rs

新增命令：

- export_client_snapshot。
- migration_queue_status。

apply_server_migration 不需要经过 Tauri，可由前端直接调用 HTTP。

迁移完成后继续使用现有 import_server_snapshot 重建本地镜像。

export_client_snapshot 只读本机数据库，不包含敏感信息。

### 10.7 src/lib/api.ts

新增：

    previewMigration(input: MigrationRequest): Promise<MigrationPreview>;
    applyMigration(input: ApplyMigrationRequest): Promise<MigrationReport>;

### 10.8 src/App.tsx

新增状态机：

    server_connecting
      ├─ onboarding_choice
      │    ├─ use_server_data
      │    │    └─ existing_import_dialog
      │    └─ migrate_client_data
      │         ├─ migration_preview
      │         ├─ migration_confirm
      │         ├─ migration_applying
      │         └─ refresh_local_mirror
      └─ connected

要求：

- 迁移期间禁用日程、订阅和设置写入。
- 迁移失败保留弹窗和错误信息，允许重试。
- 成功后关闭弹窗，刷新订阅、直播状态和日程。
- 切换服务器 URL 时重新判断是否需要 onboarding。
- Web-only 模式没有本地数据库，只显示“使用服务端数据”，不显示迁移入口。

### 10.9 src/components/SettingsView.tsx

增加“数据迁移”区块：

- 显示当前服务器 URL。
- 显示上次迁移时间。
- 提供“重新从服务器导入”。
- 提供“把本机数据迁移到服务器”。
- 当前离线队列非空时禁用迁移并说明原因。

## 11. 客户端 ID 与同步队列

### 11.1 迁移前

迁移前要求 sync_queue 没有 pending、syncing 或 blocked 记录。

原因：

- 离线 mutation 中的 ID 属于旧服务端或本地数据库。
- 直接迁移会导致 ID 指向错误。
- 同时执行迁移和队列补交会产生不可预测的覆盖顺序。

### 11.2 迁移后

迁移完成后：

1. 拉取服务端快照。
2. 调用现有 import_server_snapshot。
3. 更新 remote_entity_map。
4. 清空旧服务器下已完成的离线队列。
5. 前端重置事件序号并重新拉取活动数据。

### 11.3 后续离线操作

迁移完成后产生的离线操作必须使用服务端新 ID。

为进一步降低切换服务器时的风险，可增加：

- sync_queue.server_key 列。
- 入队时记录当前服务器 URL。
- 上传时只处理当前服务器对应的队列。

该字段可作为本计划的第二阶段增强项。

## 12. 并发与一致性

### 12.1 重试

同一 migrationId 和同一请求体：

- 返回第一次成功结果。
- 不重复写入订阅、日程或事件。

同一 migrationId 不同请求体：

- 返回 MIGRATION_ID_REUSED。

### 12.2 预览过期

应用前重新计算摘要：

- 不一致时拒绝提交。
- 客户端重新预览后再提交。

### 12.3 并发写入

- 迁移使用 BEGIN IMMEDIATE。
- 单用户服务端仍可能有后台开播任务写入。
- 迁移只锁数据库写事务，不阻塞只读请求。
- 事务失败整体回滚。

### 12.4 事件

迁移提交后写入 data_imported：

    {
      "migrationId": "uuid",
      "clientId": "uuid",
      "counts": {
        "subscriptions": 10,
        "schedules": 100
      }
    }

前端在 handleServerEvent 中处理：

- 收到 data_imported 后刷新订阅、日程和固定周表。
- 不触发开播通知。

## 13. 安全要求

- 迁移接口和现有 API 使用同一个 Bearer Token。
- 不接受敏感字段。
- 对写接口做严格长度、格式和引用完整性校验。
- 直播间 URL 只允许 https://live.bilibili.com/。
- 图片 URL 继续限制允许的 B 站图床。
- 禁止把快照中的任意 JSON 当作 SQL 或配置键执行。
- 请求日志不得打印完整快照。
- 错误日志只打印实体计数和 migrationId。
- 迁移接口不返回 Cookie、Key 或本地绝对路径。
- 迁移完成后可选返回备份文件名，但不能返回服务器文件系统绝对路径。

## 14. 测试计划

### 14.1 Core 单元测试

- 空服务端迁移空客户端快照。
- 全新客户端迁移到空服务端。
- 相同 MID 的订阅冲突。
- client 策略覆盖服务端订阅。
- server 策略保留服务端订阅。
- 日程按 source_identity 匹配。
- 无 source_identity 时按开始时间加标题匹配。
- 固定周表按 rule_key 匹配。
- 周表例外正确迁移。
- 团播多来源正确迁移。
- 来源抑制记录正确迁移。
- 手工锁定日程不会被默认覆盖。
- 客户端 ID 到服务端 ID 映射正确。
- 快照引用缺失时整体失败。
- 快照超过限制时失败。
- 同一 migrationId 重试幂等。
- 同一 migrationId 不同请求体被拒绝。
- 预览过期时拒绝提交。
- 事务中途失败后无部分数据。
- 迁移事件只写一次。

### 14.2 Server API 测试

- 未带 Token 返回 401。
- 预览接口不写数据库。
- 应用接口成功返回统计。
- 请求体超限返回明确错误。
- 非法快照返回 400。
- 预览过期返回冲突错误。
- 重试返回相同结果。
- WebSocket 收到 data_imported。

### 14.3 Tauri 测试

- export_client_snapshot 不包含敏感字段。
- 离线队列非空时拒绝导出迁移。
- 迁移完成后 remote_entity_map 正确建立。
- 客户端快照可序列化为稳定 JSON。
- 大快照能明确报错。

### 14.4 前端手工验收

- 首次连接出现两个顶层选项。
- “使用服务端数据”不调用迁移写接口。
- “迁移本机数据到服务端”显示预览和冲突数。
- 选择“保留服务端”只导入缺失项。
- 选择“客户端覆盖”后服务端数据正确变化。
- 覆盖服务端手工锁定日程需要二次确认。
- 迁移失败弹窗可重试。
- 成功后可正常查看日程、主播和固定周表。
- 重启客户端后不重复弹出初始化向导。
- 设置页可主动重新迁移。
- Web-only 模式不显示客户端迁移入口。

### 14.5 端到端场景

1. 空服务器加有本地数据客户端：
   - 迁移后服务端与客户端业务集合一致。
   - 日程来源和手工锁定保持。
2. 有服务端数据加有本地数据客户端：
   - 冲突预览准确。
   - 两种冲突策略结果符合预期。
3. 迁移过程中网络断开：
   - 客户端重试同一 migrationId。
   - 服务端只提交一次。
4. 迁移前后台开播任务写入：
   - 不破坏迁移事务。
   - 事件按序号推送。
5. 迁移后断线手工新增日程：
   - 恢复后 mutation 使用服务端 ID 正常提交。

## 15. 实施阶段

### Phase 0：契约与兼容

- [ ] 确认迁移数据范围。
- [ ] 定义 DataSnapshot schema v1。
- [ ] 定义预览和报告 JSON。
- [ ] 确认请求体上限和冲突明细截断规则。
- [ ] 确认“保留服务端”与“客户端覆盖”的字段语义。

### Phase 1：Core 快照与冲突

- [ ] 新增数据模型。
- [ ] 扩展快照读取和校验。
- [ ] 实现统一冲突键。
- [ ] 实现预览。
- [ ] 补充 Core 单元测试。

### Phase 2：服务端事务导入

- [ ] 新增 migration_receipts。
- [ ] 实现服务端导入事务。
- [ ] 实现 ID 映射。
- [ ] 实现幂等和预览摘要。
- [ ] 实现迁移事件。
- [ ] 补充 Core 集成测试。

### Phase 3：HTTP API

- [ ] 新增 /api/migration/preview。
- [ ] 新增 /api/migration/apply。
- [ ] 挂载鉴权和请求体限制。
- [ ] 补充 API 测试。
- [ ] 补充错误码文档。

### Phase 4：桌面端导出与 API 客户端

- [ ] 新增 export_client_snapshot Tauri 命令。
- [ ] 新增迁移队列状态检查。
- [ ] 新增前端 previewMigration 和 applyMigration 方法。
- [ ] 迁移后复用现有本地导入刷新 ID。

### Phase 5：首次连接向导

- [ ] 新增首次连接状态机。
- [ ] 新增“使用服务端数据 / 迁移本机数据 / 取消”选择。
- [ ] 新增迁移冲突预览面板。
- [ ] 新增覆盖手工锁定日程的二次确认。
- [ ] 新增迁移中禁用写操作。
- [ ] 新增迁移成功和失败反馈。
- [ ] 设置页增加重新迁移入口。
- [ ] 记录 onboarding 完成状态。

### Phase 6：联调与发布

- [ ] 桌面端和本地服务端联调。
- [ ] Docker 服务端联调。
- [ ] 迁移重试和断网恢复验证。
- [ ] WebSocket 刷新验证。
- [ ] 更新 README 和验收文档。
- [ ] 生成 Windows 安装包和服务端构建物。

## 16. 粗略工作量

按当前代码规模估算：

| 阶段 | 预计 |
| --- | --- |
| Phase 0 | 0.5 天 |
| Phase 1 | 1.5 天 |
| Phase 2 | 2 天 |
| Phase 3 | 1 天 |
| Phase 4 | 0.5 天 |
| Phase 5 | 1.5 天 |
| Phase 6 | 1 天 |
| 合计 | 约 8 个工程日 |

该估算不包含真实公网服务器长时间压测和正式证书部署。

## 17. 验收标准

以下条件全部满足才算完成：

1. 首次连接新服务器时，桌面端明确显示“使用服务端数据”和“迁移本机数据到服务端”。
2. 选择“使用服务端数据”时，不调用任何服务端迁移写接口。
3. 选择“迁移本机数据到服务端”时，必须先预览冲突，再提交。
4. 服务端数据只能由用户明确确认后修改。
5. 冲突策略正确区分“客户端覆盖”和“保留服务端”。
6. 服务端导入在单事务内完成，失败无部分写入。
7. 同一 migrationId 重试不会重复插入。
8. 预览后服务端数据变化会被检测并拒绝旧预览提交。
9. 订阅、官号、日程、固定周表、周表例外和多来源关系均正确迁移。
10. 手工锁定、取消状态和来源抑制语义保留。
11. 不迁移 Cookie、Token、API Key 或本地绝对路径。
12. 离线队列非空时禁止迁移。
13. 迁移完成后客户端 ID 映射正确更新，离线同步正常。
14. 迁移完成后其他客户端能收到事件并刷新数据。
15. 重启后不会重复弹出初始化向导。
16. Core、Server API、Tauri 和前端验收测试全部通过。

## 18. 风险与缓解

| 风险 | 缓解 |
| --- | --- |
| 快照较大导致请求失败 | 32 MiB 限制、客户端预检查、明确错误码 |
| 迁移覆盖服务端手工数据 | 预览冲突、二次确认、默认保留服务端 |
| 预览后服务端变化 | serverStateDigest |
| 网络中断导致重复提交 | migrationId 幂等回执 |
| 事务中途失败 | BEGIN IMMEDIATE 加整单回滚 |
| 迁移后 ID 错乱 | 迁移后重新拉取快照并重建 remote_entity_map |
| 旧日程重新出现 | 迁移来源抑制和取消状态 |
| 旧客户端兼容 | 新字段默认值、旧快照字段平铺兼容 |
| 多实例并发迁移 | 当前产品限定单实例；后续可增加数据库级迁移锁 |

## 19. 回滚方案

- 功能上线前备份服务端数据库。
- 迁移接口采用新增表，不修改旧表结构。
- 如果需要回退程序：
  - 恢复旧服务端可执行文件。
  - 保留 migration_receipts 表不会影响旧版本。
  - 如数据已错误覆盖，使用迁移前 VACUUM INTO 备份恢复数据库。
- 客户端回退：
  - 保留本地数据库。
  - 停止使用新迁移入口。
  - 按旧版逻辑继续使用服务端快照导入。

## 20. 推荐默认决策

为减少实现歧义，建议直接采用以下默认值：

- 首次连接默认推荐“使用服务端数据”。
- 迁移默认策略为“保留服务端，只导入缺失项”。
- “客户端覆盖服务端”必须再次确认。
- 不迁移非敏感设置，设置继续由设置页单独管理。
- 迁移前自动检查离线队列，非空则阻止。
- 迁移提交前服务端自动创建数据库备份。
- 迁移完成后客户端必须重新拉取服务端快照，不直接信任本地生成的映射。
