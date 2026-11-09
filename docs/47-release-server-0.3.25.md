# 服务端 0.3.25

## 症状

线上日志里周期性出现：

```
WARN oshical_core::service: post analysis failed error=网络请求失败: error sending request for url (https://…/v1/chat/completions)
```

单次全量刷新里这类失败能占到几十条，每条都要等满 90 秒超时才进入下一条，于是一次刷新被拖到十几分钟。

## 根因

`LlmClient` 的 HTTP 客户端带 90 秒总超时与 10 秒连接超时，但 `request_analysis` 里
`builder.json(&request).send().await?` 一次失败就直接把错误抛给上层，没有传输层重试。
网关偶发断连（连接被重置 / TLS 提前关闭）时，单条动态只能整轮放弃，下一轮刷新再重新等一遍。

## 改动

- `crates/core/src/llm.rs`：新增 `const LLM_SEND_ATTEMPTS: u32 = 3;`，
  `request_analysis` 的发送改成循环 `builder.try_clone()…send()`，
  命中 `reqwest::Error` 且不是构造错误时退避 2 秒重试，最多 3 次；
  HTTP 状态码（401/403/404/5xx）不重试，仍然一次性报错并记录。
- 每次重试 `tracing::warn!(error, attempt, max_attempts, "日程模型请求发送失败，稍后重试")`，
  便于在日志里区分"重试后成功"和"真失败"。

## 测试

- `cargo test -p oshical-core --locked` → 206 passed / 0 failed / 6 ignored
- `cargo test --workspace --locked` → 全绿（core 206 / server 23 / 集成 37 / desktop 14）
- `cargo clippy --workspace --all-targets` → 无告警
- `cargo fmt --all -- --check` → 通过

## 升级

```bash
docker pull ghcr.io/yuetc/oshical-server:0.3.25
docker stop oshical-test && docker rename oshical-test oshical-test-prev-0321
docker run -d --name oshical-test ... ghcr.io/yuetc/oshical-server:0.3.25
```

## 已知取舍

- 重试只覆盖传输层；模型返回非 JSON、HTTP 错误码仍然一次失败。这类失败下一轮（12 小时窗口
  之外的动态）会自然重试。
- 每次重试会再占用一次账号并发许可，最坏情况下单条动态耗时约 3×90 秒；仍在同账号串行语义内，
  不会放大并发。
