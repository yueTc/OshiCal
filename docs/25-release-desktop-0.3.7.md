# OshiCal Desktop 0.3.7

本次仅发布桌面端，标签为 `desktop-v0.3.7`。服务端保持 `0.3.13`，不再重复发布。

## 双击日程卡片不再跳进陌生直播间

B 站动态里的直播预约卡片长这样：

```json
"additional": { "type": "ADDITIONAL_TYPE_RESERVE",
  "reserve": { "rid": 5866502, "jump_url": "https://live.bilibili.com/22625027", "stype": 2 } }
```

解析器把 `rid` 当成了房间号，拼出 `https://live.bilibili.com/{rid}`。`rid` 是**预约 id**，它的号段和真实房间号重叠，于是每条预约日程都落进了别人的房间——线上库里 23 条预约日程全部错位，例如乃琳 11-08「【突击】嘿嘿影之刃零！」写成 `5866502`：

| room_id | 归属 UID | 标题 |
| --- | --- | --- |
| 5866502（错） | 3080943 | `:3`（单机游戏区，42 关注） |
| 22625027（乃琳真房间） | 672342685 = 乃琳Queen | 【突击day7】影之刃零开荒一周目最高难度 |

改法：

- `ReserveCard.room_id` 只由 `jump_url` 解析得出，`rid` 不再参与；支持 `live.bilibili.com/{房间号}`、协议相对写法，以及把房间号放在 query 里的 blackboard 活动页。
- 反序列化时忽略历史缓存里的旧 `roomId`（那其实是 `rid`），避免旧数据把错误地址再带回来。
- 取不到可信房间号时回退到主播自己的直播间，不再拼一个凭空猜测的房间。
- 历史库里的错位地址由 `repair_misattributed_reserve_room_urls` 清理：只清「房主就是所属主播、地址却不是该主播直播间」的预约日程，团播指向他人直播间的记录原样保留；桌面端和服务器启动、每次刷新都会跑，幂等，并写入同步变更推给其他设备。

## 断流重开不再拆成两场（随包携带 server 0.3.13 的核心改动）

- 断流后重新推流的 `live_time` 会让旧逻辑新建一条场次，同一场直播被切成两段，卡片按「开始时间最近」只关联到断流前那几分钟。现在 10 分钟（`LIVE_SESSION_MERGE_MINUTES`）内同主播同房间的重开并回上一行，`segment_count + 1`，同一个 `live_time` 反复轮询幂等。
- 短断流不再把 `danmaku_count` / `revenue_cents` / `duration_seconds` / `peak_*` 清零，改为保留计数器继续累加。
- `CollectorStats::from_session` 不再用 `collector_state == "active"` 猜「是不是续开的旧场次」——全新场次建行时状态本来就是 `active`，导致 0.3.11 之后每场都被标成 `partial`。现在由 `start()` 用 `updated_at > created_at` 判定。
- 直播数据面板徽标写清降级原因：`部分缺失 · 断流 1 次` / `部分缺失 · 重连 3 次`，悬停说明给出断流次数与「断线窗口内的弹幕和礼物没有计入」。

## 兼容性

- 数据模型、IPC 命令、HTTP 接口都没有破坏性变化；0.3.5/0.3.6 的本地数据库可直接升级。
- `live_sessions` 新增 `segment_count`、`last_segment_started_at` 两列，启动迁移自动补齐；`LiveSession.segmentCount` 带 `serde(default = 1)`，旧客户端收到不带该字段的 payload 仍能解析。
- 已经落库的拆分场次不会自动合并，只对本次之后的断流生效。
- 连远端服务端时，日程数据由服务器权威：服务器也要升到含本次修复的版本，历史错位地址才会被清理。

## 验证

- 本机（用户当次明确要求在本机构建）：`tauri build` 通过，前端 `tsc -b && vite build`（2545 模块）通过，`oshical-core` / `oshical-desktop` release 编译 1m27s；`rustfmt --check` 对全部改动文件通过。
- 按 `AGENTS.md`，Clippy（`-D warnings`）、`cargo test`、NSIS 安装包、更新清单与签名由 GitHub Actions（`.github/workflows/release.yml`）构建验证。
- 本地无 `TAURI_SIGNING_PRIVATE_KEY`，本机产出的 exe/setup 不含 `.sig` 与 `latest.json`；自动更新资产只由 CI 出。
- 新增测试：`reserve_room_id_comes_from_live_room_url_only`、`reserve_card_ignores_legacy_rid_room_id_on_deserialize`、`reservation_without_room_url_yields_no_room_id`、`repair_clears_reservation_room_urls_that_belong_to_someone_else`。

## 发布资产

`OshiCal_0.3.7_x64-setup.exe`、`.sig`、`latest.json`（由 CI 生成）。
