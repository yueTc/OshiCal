# OshiCal 服务端 0.3.19

## 这次修的问题

明前奶绿 2026-11-09 发的动态

> 八点来啵一下电台噢  ᖰ⌯'▾'⌯ᖳ
> 八点见八点见[怪怪小黄豆_祈祷]

同时带一张表情包。正文里写得很清楚「八点见」，日历上却没有这一场。

数据库里的证据：这条动态（`1268658714237206552`）在 `image_analyses` 里有一条记录，
`result_json` 是空数组 `[]`；`text_analyses` 里**一条记录都没有**，`schedules` 里也没有
对应场次。也就是说，正文从来没有送给日程模型。

## 根因

`process_source_posts_with_progress` 里，图片通道和文字通道是互斥的：

```rust
let has_combined_images = post.reserve.is_none()
    && llm_config.is_some()
    && settings.auto_analyze_images
    && !post.images.is_empty();
...
if !has_combined_images && !text.is_empty() {
    // 文字通道
}
```

只要动态带图且开了图片分析，文字通道就整条跳过。而图片通道虽然把正文一起送给模型
（`analyze_image_bytes` 的 user message 里有「动态文字描述」），图片侧的提示词却把
主次写反了：

> 图片负责提取日期、时间、主题和图中人员；随附动态文字负责补充或纠正……

于是模型遇到表情包、封面这类「图里没有日程」的图片时，返回空数组就收工，正文里的
「八点见」被一起丢掉。全库 346 条图片分析里 329 条是空数组，绝大多数都是这种情形；
剩下有结果的 17 条几乎都是日程表长图。

同类受害的还有：呜米「晚上8点，来玩双人闯关游戏！」、栞栞「明天早上见呀」、
明前奶绿「今天大概不来了，下周六播见」。全库「有图但没有文字分析」的动态共 85 条。

## 改动

### 1. 两条通道改成并集

`crates/core/src/service.rs`：

- 删掉 `has_combined_images` 这个互斥判断，文字分支的条件只剩 `!text.is_empty()`；
- 图片分支保持不变，仍然把正文一起送模型；
- 文字通道先跑，图片通道随后跑，两路事件按时间槽合并。

### 2. 同一场不写两遍

新增 `llm_events_equivalent(left, right)`：

- `action` 必须相同；
- `start_at` 落在 `GROUP_DUPLICATE_TOLERANCE_MINUTES`（5 分钟）以内；两侧都为 `None`
  视为相等；
- 参与者按 `normalize_alias` 比对，完全一致或「重合人数的一半以上」视为同一场。

文字通道先把事件记进 `text_events_seen`，图片通道逐条比对，命中就跳过。保留文字通道
那一条的理由：正文是作者自己写的，措辞更准，`source_kind` 也仍然是 `text`。

### 3. 图片提示词改主次

`crates/core/src/llm.rs` 的图片 system prompt 从「图片负责提取……文字补充」改成：

> 图片和随附动态文字都可能是主信息来源。任一侧出现明确的日期或时间点都要提取：文字说
> 「今晚八点」「八点见」「下周六播见」而图片里没有日期信息时，按动态发布时间所在自然日
> （或文字明确给的那一天）解释成那个时刻，不要因为图片里没有时间就返回空数组……

### 4. 空结果不再当缓存命中

图片模型经常返回 `[]`，旧逻辑把它当成「已分析」写进缓存，于是这条动态永远拿不到结论。
现在：

- 内存缓存只在 `!cached.events.is_empty()` 时命中；
- 数据库缓存读出来是空数组时，打一条 `debug`（`cached image analysis is empty, retry`）
  后重新调用模型。

### 5. 版本号

- 文字通道 `prompt_version` 从 `*-text-v11` 升到 `*-text-v12`；
- 图片通道从 `*-image-context-v5-*` 升到 `*-image-context-v6-*`。

带图动态历史上被跳过的正文这一轮会全部重新分析；图片侧 329 条空结果同样失效。
`image-context-v6` 仍然拼接正文摘要（`text_digest` 前 12 位），正文改动后图片结果会重算。

## 测试

新增：

- `image_dynamic_still_runs_the_text_channel`：用这条事故动态的真实 id 与图片 URL，图片
  模型返回 `[]`、文字模型给出「明天 20:00 电台」，断言两条通道各调一次、日历落一条
  `source_kind = text` 的日程。
- `image_event_duplicating_the_text_event_is_dropped`：两路给出同一时间槽、标题措辞不同，
  断言只落一条，标题取文字通道那一条。
- `empty_cached_image_analysis_is_retried`：库里预置空数组缓存，断言不算缓存命中、模型
  真的被调用一次。
- `unsupported_image_skip_is_not_an_error`：动图返回 `Ok(None)`，断言不记 `analysis_errors`。
- `llm_events_equivalent_matches_slot_and_participants`：时间槽、参与者、`action` 三个维度
  的判定。

调整：

- `shared_official_image_calls_model_once`：带图动态现在也会跑文字通道，`text_calls`
  从 0 改为 1，`llm_calls` 从 1 改为 2。

既有测试全部保持通过；`cargo fmt --all -- --check`、`cargo clippy -D warnings`、
`cargo test --workspace --locked` 由 CI 执行。

## 升级

```bash
docker compose pull
docker compose up -d
```

升级后触发一次刷新即可。带图动态会重新走一遍文字通道，历史上漏掉的场次会补进日历。

## 已知取舍

带图动态现在会多一次文字模型调用。已经命中 `text-v12` 缓存的动态不会重复计费；图片和
正文指向同一场时只落一条，不会因为两路都成功而出现重复场次。
