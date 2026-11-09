# 服务端 0.3.22 发布说明

全量刷新的任务进度一直停在「准备全量刷新 0%」。原因是 `refresh_job_all` 只播报了一次初始进度，随后调用的 `AppService::refresh_all()` 没有进度回调，`refresh_sources` 里只有「单个主播刷新」那条路径会 `emit_progress`。一次全量刷新要十几分钟，界面上却完全看不到进展，看起来像卡死。

## 改动

- 新增 `AppService::refresh_all_with_progress(on_progress)`，`refresh_all()` 变成它的无回调包装。
- 进度目标从 `Option<&Streamer>` 换成 `Option<&ProgressTarget>`：单个主播刷新用真实主播，全量刷新用合成条目（`id = 0`、`name = "全部主播"`），因此全量刷新也会按「拉取动态 3/16」这样推进百分比。
- `server/src/api.rs` 的 `refresh_job_all` 接上 `jobs.update_streamer_progress`，任务卡实时更新 stage/completed/percent。
- 用例 `all_refresh_progress_uses_synthetic_target`：合成条目播报后 `streamer_id == 0`、`streamer_name == "全部主播"`、百分比原样透传。

## 测试

`cargo test --workspace --locked`：core 204 passed / 6 ignored，server 23、集成 37 全绿；`cargo clippy --workspace --all-targets` 无告警；`cargo fmt --all -- --check` 通过。

## 升级

```bash
docker pull ghcr.io/yuetc/oshical-server:0.3.22
```

替换容器时沿用原参数（`-p 127.0.0.1:18787:8787`、`-v /opt/oshical-test/data:/app/data` 等）。升级前备份 `oshical.db`。
