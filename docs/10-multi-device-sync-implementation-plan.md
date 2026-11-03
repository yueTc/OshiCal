# 多设备同步实现计划 V2

- 状态：V2 收口完成，离线编辑、迁移幂等和首次接入状态已落地
- 日期：2026-10-29
- 设计依据：docs/09-server-authoritative-multi-device-sync.md
- 迁移子流程：docs/08-client-server-migration-plan.md

## 1. 本轮目标

本轮先落地服务端权威同步的最小完整闭环：

1. 服务端正式写入产生连续 sync_changes。
2. 服务端提供 bootstrap 和 changes 接口。
3. WebSocket 支持 protocolVersion 2、sync_batch、ACK 和 reset_required。
4. 客户端保存 cursor，按批次原子应用，缺口时通过 HTTP 补拉。
5. 设备注册和最后同步位置可查询。
6. 手动刷新任务通过服务端执行，所有设备可收到进度。
7. 服务器模式下本地采集保持关闭，托盘刷新改走服务器。
8. 通知和窗口设置保持设备本地。

本轮不要求完成：

- 每设备独立 Token。
- 多用户权限系统。
- 公网大规模压测。
- 历史直播统计全量迁移。
- 完全替换旧事件协议；旧协议继续兼容。

## 2. 共享协议

### 2.1 Rust 类型

在 crates/core/src/models.rs 增加：

    pub struct SyncChange {
        pub seq: i64,
        pub entity_type: String,
        pub entity_id: String,
        pub operation: String,
        pub entity_revision: i64,
        pub payload: Option<serde_json::Value>,
        pub created_at: DateTime<Utc>,
    }

    pub struct SyncChangesPage {
        pub schema_version: u32,
        pub from_seq: i64,
        pub to_seq: i64,
        pub latest_seq: i64,
        pub min_retained_seq: i64,
        pub has_more: bool,
        pub reset_required: bool,
        pub changes: Vec<SyncChange>,
    }

    pub struct SyncBootstrap {
        pub schema_version: u32,
        pub snapshot_revision: i64,
        pub min_retained_seq: i64,
        pub server_time: DateTime<Utc>,
        pub subscriptions: Vec<Streamer>,
        pub schedules: Vec<Schedule>,
        pub weekly_rules: Vec<WeeklyScheduleRule>,
        pub weekly_exceptions: Vec<WeeklyScheduleException>,
        pub schedule_sources: Vec<ScheduleSource>,
        pub source_suppressions: Vec<SourceSuppression>,
        pub server_settings: serde_json::Value,
        pub live_statuses: Vec<LiveStatus>,
        pub live_sessions: Vec<LiveSession>,
    }

    pub struct DeviceInfo {
        pub device_id: String,
        pub name: String,
        pub platform: String,
        pub app_version: String,
        pub last_seen_at: DateTime<Utc>,
        pub last_ack_seq: i64,
        pub online: bool,
    }

### 2.2 TypeScript 类型

在 src/types.ts 增加：

    export interface SyncChange {
      seq: number;
      entityType: string;
      entityId: string;
      operation: "upsert" | "delete";
      entityRevision: number;
      payload: unknown | null;
      createdAt: string;
    }

    export interface SyncChangesPage {
      schemaVersion: number;
      fromSeq: number;
      toSeq: number;
      latestSeq: number;
      minRetainedSeq: number;
      hasMore: boolean;
      resetRequired: boolean;
      changes: SyncChange[];
    }

    export interface SyncBootstrap {
      schemaVersion: number;
      snapshotRevision: number;
      minRetainedSeq: number;
      serverTime: string;
      subscriptions: Streamer[];
      schedules: Schedule[];
      weeklyRules: WeeklyScheduleRule[];
      weeklyExceptions: WeeklyScheduleException[];
      scheduleSources: ScheduleSource[];
      sourceSuppressions: SourceSuppression[];
      serverSettings: ServerSettings;
      liveStatuses: LiveStatus[];
      liveSessions: LiveSession[];
    }

### 2.3 WebSocket 消息

    auth:
    {
      "type": "auth",
      "token": "...",
      "deviceId": "...",
      "protocolVersion": 2
    }

    auth_ok:
    {
      "type": "auth_ok",
      "protocolVersion": 2,
      "deviceId": "...",
      "latestSeq": 123,
      "minRetainedSeq": 0,
      "resetRequired": false
    }

    sync_batch:
    {
      "type": "sync_batch",
      "fromSeq": 100,
      "toSeq": 105,
      "latestSeq": 110,
      "changes": []
    }

    ack:
    {
      "type": "ack",
      "seq": 105
    }

    reset_required:
    {
      "type": "reset_required",
      "reason": "cursor_expired",
      "latestSeq": 123,
      "minRetainedSeq": 50
    }

## 3. 工作包

### WP1：Core 同步存储

负责人：智能体 A

写入范围：

- crates/core/src/models.rs
- crates/core/src/store.rs
- crates/core/src/store/sync_feed.rs
- crates/core/src/lib.rs

任务：

- 新增 sync_changes、devices 表。
- 增加业务实体 revision 字段和回填。
- 实现 append_sync_change。
- 实现 latest_sync_seq、min_retained_sync_seq。
- 实现 sync_changes_after。
- 实现 sync_bootstrap 数据读取。
- 实现 device register/upsert/list/ack。
- 实现 envelope helper，允许一个事务写实体和 change。
- 为 schedules 的增删改接入 sync_changes。
- 为 subscriptions 和 linked_accounts 接入 sync_changes。
- 为 weekly rules 接入 sync_changes。
- 为 live_status 接入 sync_changes。
- 保留旧 event_journal 兼容。
- 增加 Core 测试。

完成标准：

- 新增实体后有连续 seq。
- 删除有 tombstone。
- 事务失败没有 change。
- bootstrap revision 和 changes 能衔接。
- cargo test -p oshical-core 通过。

### WP2：Server 同步 API 与 WebSocket v2

负责人：智能体 B

写入范围：

- server/src/lib.rs
- server/src/api.rs
- server/src/sync_api.rs
- server/tests/api.rs
- server/tests/sync_api.rs

任务：

- 新增 GET /api/sync/bootstrap。
- 新增 GET /api/sync/changes。
- 新增 POST /api/sync/ack。
- 新增 POST /api/devices/register。
- 新增 GET /api/devices。
- WebSocket 支持 protocolVersion 2 和 deviceId。
- 实现 sync_batch 推送。
- 实现 ACK 处理。
- 实现 reset_required。
- 保留旧 auth 和旧事件兼容。
- 增加 API 测试。

完成标准：

- 旧客户端仍能连接。
- 新客户端能完成 bootstrap、changes、ack。
- 两台模拟客户端收到同一批变更。
- cargo test -p oshical-server 通过。

### WP3：Client 同步引擎与 Tauri 桥

负责人：智能体 C

写入范围：

- src-tauri/src/lib.rs
- src-tauri/src/sync_client.rs
- src/types.ts
- src/lib/api.ts

任务：

- 生成并持久化 device_id。
- 新增 get_device_id 命令。
- 新增 sync_meta 表或等价持久化。
- 新增 bootstrap 应用命令。
- 新增 apply_sync_batch 命令，实体更新和 cursor 更新同事务。
- 新增 get_sync_cursor、set_sync_ack 命令。
- WebSocket v2 客户端支持 sync_batch 和 ack。
- 缺口时调用 changes 补拉。
- reset_required 时调用 bootstrap。
- 托盘刷新在服务器模式下调用服务器任务接口。
- 保留旧事件转发兼容。
- 增加最小 Rust/TypeScript 单元验证。

完成标准：

- 断线重连从 cursor 继续。
- batch 应用失败 cursor 不前进。
- reset 后能重建缓存。
- 服务器模式不启动本地采集。
- 托盘刷新不调用本地 refresh_all。

### WP4：前端同步状态与设备 UI

负责人：智能体 D

写入范围：

- src/App.tsx
- src/components/SettingsView.tsx
- src/components/DeviceSyncPanel.tsx
- src/styles.css
- src/lib/sync.ts

任务：

- 增加同步状态指示：connected、syncing、out-of-sync、error。
- 处理 sync_batch、reset_required、job_progress。
- 收到正式变更后更新前端数据，不再每次整表刷新。
- 设置页增加设备列表和同步状态。
- 设备本地设置与服务端设置分区。
- 通知开关保持本地。
- 首次连接和迁移入口接入现有 pendingImport 流程。
- 刷新进度跨设备展示。
- 增加必要类型和空状态。

完成标准：

- 两台设备界面能实时同步日程变化。
- 通知开关不会修改服务器。
- 显示当前 cursor 和最新 seq。
- 迁移完成后能正常进入服务器模式。

## 4. 集成顺序

1. WP1 先合入共享类型和表。
2. WP2 基于 WP1 接口合入 HTTP/WS。
3. WP3 基于 WP2 协议合入客户端游标。
4. WP4 基于 WP3 事件格式合入界面。
5. 最后统一 resolve 类型和测试冲突。
6. 运行 workspace 全量检查。

## 5. 文件所有权

| 区域 | 主要文件 | 所有者 |
| --- | --- | --- |
| Core | crates/core/src/models.rs, store.rs, store/sync_feed.rs, lib.rs | WP1 |
| Server | server/src/api.rs, sync_api.rs, lib.rs, runtime.rs | WP2 |
| Client bridge | src-tauri/src/lib.rs, sync_client.rs | WP3 |
| Frontend | src/App.tsx, settings, components, lib/sync.ts | WP4 |

禁止跨包修改：

- WP1 不修改 server、src、src-tauri。
- WP2 不修改 crates/core 内部实现。
- WP3 不修改 server。
- WP4 不修改 Rust。

如果接口不足，先在最终汇报中说明，不直接改其他人的文件。

## 6. 验收命令

    cargo fmt --all -- --check
    cargo test -p oshical-core
    cargo test -p oshical-server
    cargo check --workspace
    npx tsc -b --pretty false

## 7. 风险

| 风险 | 处理 |
| --- | --- |
| 共享类型不一致 | 以本文件第 2 节为准 |
| WP1 和 WP2 同时改模型 | WP1 独占 models.rs |
| 客户端 DB schema 冲突 | WP3 使用新增表，不重建旧表 |
| 旧客户端断连 | 服务端按 protocolVersion 分支 |
| event_journal 和 sync_changes 重复 | 旧事件继续用于提示，新变更用于同步 |
| 长事务阻塞 | 单次变更批次控制在合理范围 |
| 多智能体改同一文件 | 按文件所有权执行 |

## 8. 当前实现状态

- 状态：V2 收口完成，可进入多设备联调与部署验证。
- 更新日期：2026-10-29。

### 已完成

- Core 新增 sync_changes、devices、实体 revision、mutation receipts、增量 payload 和保留策略。
- 订阅、官号、日程、固定周表、来源抑制、开播状态、服务器设置、直播场次已接入同步日志。
- 新增 bootstrap、changes、ack、devices、migration 相关 HTTP API。
- WebSocket 支持 protocolVersion 2、sync_batch、ACK、reset_required、缺口补拉。
- 服务端后台广播增量变更；旧 WebSocket 协议继续兼容。
- 本地模式数据库和每服务器 cache.db 已拆分，服务器快照可独立重建。
- Tauri 支持稳定 device_id、游标、实体缓存、快照重建、原子批次应用。
- 服务器模式断网时日程新增/修改/删除只写当前服务器 cache.db，并进入离线 mutation 队列。
- 临时负数日程在 mutation 应用后清理，并强制从服务端权威快照回同步。
- create/update/delete 的 baseRevision 冲突保护和 schedule mutationId 幂等收据已接入。
- 服务端刷新任务具备注册表、去重、进度查询和短期完成状态保留。
- 前端支持同步状态、设备列表、游标、刷新任务和离线缓存回退。
- Web 端新增 IndexedDB 实体缓存和 localStorage 游标。
- 首次接入支持“使用服务端数据 / 迁移本机数据到服务端 / 稍后”，完成状态按服务器与设备持久化。
- 迁移支持服务端预览、32 MiB 请求上限和冲突策略。
- 迁移申请支持稳定 migrationId、持久化收据、并发串行化、重试返回首次报告和内容冲突检测。
- `data_imported` 事件包含 migrationId，所有设备可据此重新同步。

### 验证结果

- `cargo fmt --all -- --check`：通过。
- `cargo check --workspace`：通过。
- `cargo test --workspace --no-fail-fast`：通过。
  - Core：85 通过，6 个联网 smoke ignored。
  - Desktop：7 通过。
  - Server unit：9 通过。
  - Server API：7 通过。
  - Server sync API：11 通过。
- `cargo clippy --workspace --all-targets -- -D warnings`：通过。
- `tsc -b --pretty false`：通过。
- `vite build`：通过，dist 已更新。

### 已知边界

- 当前仍为单用户共享 Token，没有每设备 Token、远程撤销和细粒度权限。
- 刷新任务注册表在进程内，重启后不恢复。
- Web IndexedDB 已有确定性逻辑测试，但尚未加入真实浏览器端到端集成测试。
- 旧 event_journal 继续承担通知兼容，sync_changes 是新同步主路径。
- 历史直播统计尚未全量迁移；当前迁移快照聚焦订阅、日程和固定周表。
- 迁移幂等收据依赖 SQLite 持久化；数据库损坏或恢复旧备份时仍需人工核对迁移历史。

## 9. V2 收口记录

### WP5：服务器缓存离线 CRUD

- [x] 修复 sync_client 离线日程函数位于测试模块内的结构错误。
- [x] 新增服务器 cache.db 的离线新增、修改、删除和临时负数 ID 清理。
- [x] Tauri 暴露 cache_create_schedule、cache_update_schedule、cache_delete_schedule、purge_offline_schedules。
- [x] 前端服务器模式断网时只写对应服务器缓存，不再写本地模式主库。
- [x] 完成 mutation 后清理临时日程并强制回同步服务端权威数据。

### WP6：迁移幂等

- [x] /api/migration/apply 接收稳定 migrationId。
- [x] 服务端持久化迁移收据；同 ID 同内容重试返回首次 ImportReport，不重复写 sync_changes 或 data_imported。
- [x] 同 ID 不同内容返回 MIGRATION_ID_CONFLICT。
- [x] 同一 migrationId 并发请求串行化。
- [x] 前端在预览成功后生成 migrationId，并在重试时复用。

### WP7：首次接入状态

- [x] 按 serverUrl + deviceId 持久化 onboarding 完成状态。
- [x] “使用服务端数据”“迁移本机数据”“稍后”不会在普通设置保存后重复弹窗。
- [x] 桌面端启动时检查未完成接入状态，新增设备仍能看到接入选择。

### WP8：最终验证

- [x] cargo fmt --all -- --check
- [x] cargo check --workspace
- [x] cargo test --workspace --no-fail-fast
- [x] cargo clippy --workspace --all-targets -- -D warnings
- [x] tsc -b --pretty false
- [x] vite build

### 后续高优先级

- 每设备 Token、撤销和审计日志。
- refresh job 状态跨重启恢复与失败重试策略。
- Web IndexedDB 的真实浏览器集成测试和多标签页协调。
- 历史直播统计与完整运营数据迁移。
- 公网规模下的 WebSocket 压测、慢客户端和日志保留容量评估。

