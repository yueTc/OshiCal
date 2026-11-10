# 服务端 0.3.27：删除多账号并行，并行个数回归单端点并发

0.3.20 为「不同账号互不影响」引进了模型账号池：设置里维护一组账号，每个账号各存一把密钥，同一账号串行、不同账号并行。实际用不上：部署方只有一套模型端点、一把密钥，而账号列表把「并行个数」这个真正想要的能力绕成了一个只能在多账号时才生效的参数。0.3.27 把账号池整层删掉，并行个数直接表示「同时向同一个模型端点发多少条请求」。

## 删掉了什么

- 设置项 `llm_accounts`（`SETTING_KEYS` 15 → 14，`AppSettings` / `ServerSettings` 同步去掉字段）。
- 账号级密钥：`server:secret:llm_api_key:v1:{account}` 与带账号后缀的 AAD，改回单密钥槽位 `server:secret:llm_api_key:v1`。
- `OSHICAL_LLM_ACCOUNTS` 环境变量与 `EnvLlmAccount`。
- `/api/secrets/llm`、`/api/llm/test`、`/api/llm/models` 的 `accountId` 参数。
- `LlmPool` 的账号轮询与每账号串行锁，`LlmAccount` / `LlmAccountLease` 两个类型。

## 保留与改变

- `llm_parallelism`（1..=8）仍然有效，语义改为「同一端点上的并发请求数」，直接映射成一个 `Semaphore` 的许可数。
- `llm_reasoning_effort`（auto/low/medium/high）不变，仍然只在非 `auto` 时下发 `reasoning_effort` 字段。
- `OSHICAL_LLM_PARALLELISM` / `OSHICAL_LLM_REASONING_EFFORT` 覆盖不变。
- 旧库兼容：0.3.20 ~ 0.3.26 写在 `server:secret:llm_api_key:v1:default`、AAD 带 `:default` 后缀的密文仍能读出来；下一次保存密钥（哪怕值没变）会按单密钥 AAD 重新加密写回旧键，`revision` 不涨。

## 升级

```bash
docker compose pull && docker compose up -d   # 或按 docs/32 的 docker run 方式替换镜像
```

升级后 `/api/health` 的 `version` 应为 `0.3.27`；`GET /api/secrets/llm` 仍返回 `configured: true`（密钥沿用旧密文，无需重填）。

## 测试

`cargo test --workspace --locked` → core 205 / server 23 + 集成 37 + 14 / 桌面 11 全绿。新增/改写的用例：

- `secrets::llm_secret_round_trips_with_a_single_slot`（同值不涨 revision、换值涨 1）
- `secrets::account_scoped_ciphertext_is_still_readable_and_migrates_on_write`（旧账号槽位可读并迁移）
- `llm::pool_effective_parallelism_is_at_least_one`、`service::parallelism_permits_bound_the_in_flight_requests`（并行个数就是并发许可数，0 兜底成 1）
