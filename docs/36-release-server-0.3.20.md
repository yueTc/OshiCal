# 服务端 0.3.20 发布说明

## 这一版改了什么

三件事，都是从「动态日程识别不准 / 调用太贵」这条线上下来的：

1. **一条动态只调用一次模型**。以前是「文字一次 + 每张图各一次」，9 图动态要发 10 次请求；现在把正文和全部图片塞进同一个请求，一条动态永远只有 1 次调用。
2. **思考程度可选**。设置里能选「自动 / 低 / 中 / 高」，自动时不发送 `reasoning_effort` 字段，行为和以前一致。
3. **模型账号池**。可以配多个账号（各自的 Base URL、模型、密钥），不同账号之间并行、同一账号内部串行。

## 为什么改成单次调用

0.3.19 之前，动态被拆成两条通道：

- 文字通道：只送正文，`text:{post_id}:{digest}:{idx}`。
- 图片通道：每张图各调一次，`image:{post_id}:{digest}:{idx}`。

问题有三个：

- **成本**：九宫格动态要发 10 次请求，每次都把同一段正文重新送一遍。
- **一致性**：两条通道各自出事件，图片通道常常给出错误的 performer（把表情包里的名字当主播），要靠 5 分钟时间窗 + 参与者交集去重。
- **上下文缺失**：图片通道只送正文、不送发布者，模型在 `reasoning_content` 里反复纠结「无法确认主播」，最后返回 `[]`。

改成单次之后，正文、发布者、发布时间、预约卡、全部图片在同一个 prompt 里，模型不需要在两份材料之间猜。

实测（15 条真实动态，含 1 图、9 图、纯文字、预约卡）：

| 指标 | 双通道（0.3.19） | 单次调用（0.3.20） |
| --- | --- | --- |
| 调用次数 | 38 | 15 |
| 明前奶绿「八点见」 | 2 次调用 / 2 事件（其中 1 条主播错成表情包名） | 1 次调用 / 1 事件，主播正确 |
| 枝江周表 | 2 次 / 12 事件，主播是简称 | 1 次 / 12 事件，主播是完整昵称 |
| 九宫格动态 | 10 次 / 0 事件 | 1 次 / 0 事件 |
| credit | 约 2.65 | 约 2.12 |

## 为什么加思考程度

`reasoning_effort=high` 在网关上是支持的（usage 里有 `reasoning_tokens`），但对「这条动态有没有日程」这类判断几乎没有提升：同一个样本 6 次对照，high 命中 4/6，默认 3/6，延迟从 5.9 秒涨到 8.3 秒（最高 28 秒），token 从约 470 涨到 1200~3200。

所以默认是「自动」（不发送该字段），只在确有需要时手动开。UI 上直接写明这个取舍，不做默认开启。

## 行为变化（升级前请读）

- **缓存表换了**：新的 `post_analyses(cache_key, post_id, content_digest, model, prompt_version, result_json, created_at)`。`text_analyses` / `image_analyses` 保留为只读历史，不再写入。
- **缓存键变了**：`{post_id}:{content_digest}:{model}:{prompt_version}-post-v1`，`content_digest = sha256(sha256(文字上下文) + "|" + 预约卡 + "|" + 图片 URL 列表)`。换图、加图、改正文都会重新分析。
- **空结果也入缓存，但只认 12 小时**：模型返回 `[]` 会写进 `post_analyses`，重试窗口内不再重问；窗口过了（`EMPTY_ANALYSIS_RETRY_HOURS = 12`）才重新分析。以前空结果被永久缓存，等于把「没识别出来」固化成结论；完全不缓存则每轮刷新都要把上百条动态重问一遍，一次全量刷新会拖到十几分钟。
- **只在真的识别出新事件时才清旧来源**：一条动态这一轮返回空，不会把上一轮已经落库的场次及其来源删掉，避免模型偶发漏看把可用日程清空。
- **纯文字动态的 source_identity 不变**：仍是 `text:{post_id}:{text_digest}:{idx}`，历史日程不会重复建条。带图动态改成 `image:{post_id}:{content_digest}:{idx}`。
- **过期来源会被清理**：同一篇动态本轮没再产出的 identity，会从 `schedule_sources` 删掉；如果因此变成没有任何来源、且没有手动锁定，日程行也会一起删。
- **同步状态键换前缀**：`analysis-errors:v1:{streamer_id}` 里的条目从 `text:*` / `image:*` 改成 `post:{id}`，旧前缀条目在下一轮刷新时清掉。
- **`prompt_version` 升到 `*-post-v1`**：升级后所有历史动态会重新分析一遍，第一次刷新会比较慢（受 `llm_parallelism` 影响）。

## 新增环境变量

| 变量 | 说明 |
| --- | --- |
| `OSHICAL_LLM_REASONING_EFFORT` | `auto` / `low` / `medium` / `high`，非法值忽略 |
| `OSHICAL_LLM_PARALLELISM` | 1~8，超过账号数时按账号数封顶 |
| `OSHICAL_LLM_ACCOUNTS` | JSON 数组，元素 `{id,name,baseUrl,model,apiKey}`，`apiKey` 可省（省了就靠设置页里存的那份） |

密钥按账号分开存：状态键 `server:secret:llm_api_key:v1:{account_id}`，AAD 也带 `account_id`；旧键作为 `default` 账号的兜底读取，读到后会在下次写入时迁移。

## 接口变化

- `GET /api/secrets/llm?accountId=...`、`PUT /api/secrets/llm`（body 带 `accountId`）。
- `POST /api/llm/test`、`POST /api/llm/models` 接受 `accountId`、`reasoningEffort`。
- `GET /api/settings` 多返回 `llmReasoningEffort`、`llmParallelism`、`llmAccounts`。

## 测试

`cargo test -p oshical-core` 覆盖新增行为：

- `nine_images_go_in_one_request`、`one_image_goes_in_one_request`、`text_only_post_has_no_image_parts`
- `prompt_carries_author_and_publish_time`、`cancel_event_survives_single_call`
- `empty_result_is_not_cached`、`reasoning_effort_auto_omits_field`
- `text_identity_is_stable_for_text_posts`、`stale_identities_are_pruned`
- `same_account_never_runs_twice_concurrently`、`different_accounts_run_in_parallel`、`parallelism_clamped_to_account_count`
- `varying_titles_merge_into_one_slot`

## 升级

```bash
docker compose pull
docker compose up -d
```

或按现有的 `docker run` 方式替换镜像 `ghcr.io/yuetc/oshical-server:0.3.20`。升级前建议备份 `oshical.db`。

## 已知取舍

- 单次调用意味着一条动态只有一次机会：失败就是失败，不会像以前那样由第二条通道补上。失败会记进 `analysis-errors`，下一轮刷新重试。
- 9 图动态一次请求约 800 KB base64，网关并发 3 路时出现过连接重置，所以并行度按账号数封顶、同账号串行。
- `reasoning_effort` 提升有限，不要指望它治好识别问题；真正有用的是把发布者、发布时间、预约卡和图片放在同一个 prompt 里。
