# 服务端 0.3.24

## 症状

手工点「全量刷新」后，任务详情长时间停在「准备全量刷新 0% 0/1」，看不出到底在跑还是卡死；
后台轮询自己触发的那次刷新结束后，任务才突然跳到真实阶段。

## 根因

`AppService` 只有一把刷新锁。后台轮询每 300 秒跑一次全量刷新，一次要串行分析上百条动态，
经常跑十几分钟；这期间手工触发的刷新只能排队。排队过程没有任何播报
（`refresh_all_with_progress` 直接 `self.refresh_lock.lock().await`，界面拿到的还是初值），
于是看起来像卡在 0%。

## 改动

1. `crates/core/src/service.rs` 新增 `wait_for_refresh_lock`：`try_lock` 失败时每 3 秒播报
   `等待上一轮刷新结束（已等待 N 秒）`，拿到锁后自动进入正常阶段。
2. `refresh_all_with_progress` 与 `refresh_streamer_with_progress` 都改走这条路径，
   全量刷新与单主播刷新共用同一套排队提示。
3. 删掉不再被调用的 `acquire_refresh_lock`（避免两套取锁方式并存）。
4. 新增测试 `waiting_for_the_refresh_lock_reports_progress`：持锁 200ms 断言先收到排队播报。

## 测试

- `cargo test -p oshical-core --locked` → 206 passed / 0 failed / 6 ignored
- `cargo test --workspace --locked` → core 206 / server 23 / 集成 37 / desktop 14 全绿
- `cargo clippy --workspace --all-targets` → 无告警
- `cargo fmt --all -- --check` → 通过

## 升级

```bash
docker pull ghcr.io/yuetc/oshical-server:0.3.24
docker stop oshical-test && docker rename oshical-test oshical-test-prev-0323
docker run -d --name oshical-test ... ghcr.io/yuetc/oshical-server:0.3.24
```

回滚：停掉新容器，把 `oshical-test-prev-0323` 改回 `oshical-test` 再启动。

## 已知取舍

- 排队提示每 3 秒发一次进度事件，任务轮询会看到重复的阶段名（只有秒数在变），
  这是刻意的：用户需要看到"还在等"的信号，而不是一个静止的 0%。
- 排队没有超时上限。后台刷新一旦真的挂死，手工刷新会一直等下去；此时应当重启容器，
  而不是放宽锁语义。
