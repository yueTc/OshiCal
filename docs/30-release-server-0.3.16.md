# OshiCal Server 0.3.16

本次仅发布服务端，标签为 `server-v0.3.16`。桌面端保持 `0.3.9`。

0.3.16 修两个连在一起的问题：**游戏版本号被当成日期**，以及**预约卡片绕过日程模型**。
两件事叠在同一场直播上，让它从日历里消失了两天。

## 变更

### 版本号 `2.6` 被解析成 2 月 6 日，年份又顺延到 2027

思诺的动态（`1267942687349145604`，发布于 2026-11-07 20:25 北京时间）正文是：

```text
奥古斯塔是一个很有魅力的人～…感觉今晚吃饭都更香了～
下周一18:00继续2.6下哦！
```

自带的直播预约卡：

```json
{"title":"直播预约：【2D鸣潮】2.6下！","description":"今天 18:00 直播","stype":2}
```

正确结果是 **2026-11-09 18:00**（周一的直播）。实际入库的是
**2027-02-06 18:00**，所以整周日程里没有她。

链路是这样断的：

1. `parse_reserve_schedule` 把卡片标题和描述拼成一段文本：`直播预约：【2D鸣潮】2.6下！ 今天 18:00 直播`。
2. `resolve_date` 先跑「强日期」正则，`SLASH_DATE_HINT` 是 `\d{1,2}\s*[./]\s*\d{1,2}` —— 太宽，
   它把标题里的 **`2.6`** 当成了「2 月 6 日」，于是走 `parse_explicit_date`。
3. `month_day_on_publication_year` 按发布年算：2026-02-06 已经过去，就取次年 2027-02-06，
   判据是「不超过发布日 +180 天」，刚好通过。
4. 卡片时间一确定就覆盖了描述里的「今天 18:00」，文字那条正确的模型结果被同槽去重丢掉。

### 修复：日期识别与「卡片优先」

`crates/core/src/schedule.rs`：

| 位置 | 改动 |
| --- | --- |
| `has_calendar_date` | 日期检测与解析同步：`[./]` 从通用日期正则里拆出来，改由 `slash_date_regex` / `dot_date_regex` 判定，避免「检测命中但解析不到」丢事件 |
| `resolve_date` | 改成四级，并且全部以**发布时间**为日期基准：强日期 → 相对词（今天/明天/后天）→ 周几 → 点分日期 → 兜底发布日 |
| `slash_date_regex` / `dot_date_regex` | 用首尾字符排除法（Rust 正则没有 lookbehind）：`(?:^|[^0-9A-Za-z_:])` 开头、`(?:$|[^0-9A-Za-z_%])` 结尾，`2.6`、`v1.2`、`3.5%` 都不再命中 |
| `parse_dot_date` | 点分日期只在 **31 天**窗口内跨年，`2.6` 在 11 月发布不会再推到次年 |
| `parse_reserve_schedule` | 用「发布日零点」当解析锚点，卡片描述里的「今天/明天 18:00」永远相对发布时刻；解析结果早于 `now` 就直接丢弃，不再随刷新时间漂移 |
| `resolve_date` 让位规则 | 正文里同时出现「今晚吃饭」和「下周一18:00」时，相对词只落到今天、而周几能落到未来，就让位给周几 |

### 预约卡片也要交给模型

原来的闸门是「卡片能解析出时间就不跑模型」，只有正文另写了一场的动态才放行模型。
`2.6` 误判成 2027 之后，正文里的「下周一18:00」被算成同一个槽位，模型因此从未被调用。

`crates/core/src/service.rs`：

- 卡片动态一律跑文字模型，输入用 `reserve_context(post)`：正文 + 卡片标题 + 卡片描述，
  同一条动态只调用一次。
- 模型事件不再按「与卡片同槽就丢弃」二次过滤；卡片与正文指向同一场时由模型合并成一条。
- 卡片正则降级为兜底，只在三种情况补一条 `source_kind=reserve` 的日程：
  未配置模型、模型调用失败、模型返回的事件没有覆盖卡片那一场（±10 分钟）。
- 预约动态里的 `cancel` 由模型判断后采信，不再一律忽略。
- 同槽合并保持「时间归模型、标题归卡片」：`merge_input_for_existing` 仍保留已有 reserve 的
  标题与来源，只把时间换成模型给的那个——卡片标题是官方文案，文字动态里的
  「明天下午三点就来」是改期通知，不该把「【突击】影之刃零DAY5！」改名成「和奶淇琳见面」。
- prompt 版本升到 `text-v10`：明确告诉模型输入末尾两行是这条动态自带的卡片标题与描述，
  卡片时间是相对发布时间的参考值，正文另写的日期时间优先。

删除的函数：`post_reserve_start`、`post_text_has_extra_schedule_time`、
`remove_stale_model_schedules_for_reserved_post` —— 它们都建立在「卡片正则=权威」之上。

已知取舍：原来 `remove_stale_model_schedules_for_reserved_post` 会顺手清掉卡片动态下
「和卡片不同槽」的旧模型日程。模型成为主来源后这一步被去掉了——模型这次不再输出的旧场次
会留在日历里，由用户手动删除，或等下一次抓取按同一 `source_identity` 覆盖。

### 回归测试

`crates/core/src/schedule.rs`：

- `game_version_number_is_not_a_dot_date`：整条动态 → 2026-11-09 18:00 北京
- `version_number_dot_date_does_not_roll_into_next_year`
- `relative_day_wins_over_version_number`
- `reserve_relative_day_is_anchored_to_publication`
- `evening_chatter_does_not_swallow_next_week_slot`

`crates/core/src/service.rs`：

- `reserved_dynamics_analyze_card_with_the_text_model`：卡片动态确实调用模型，落 `text`
- `reserved_llm_event_takes_over_the_card_schedule`：模型标题覆盖卡片标题
- `reserved_post_schedules_both_model_events`：卡片那一场与正文第二场都由模型给出
- `reserved_post_cancellation_runs_through_the_model`
- `reserved_card_falls_back_to_regex_without_llm` / `..._when_model_returns_nothing` /
  `..._is_added_when_model_misses_the_card_slot`

## 升级

```bash
docker compose pull && docker compose up -d
```

升级后清掉那一条被写坏的旧日程，触发一次抓取即可重新落到正确时间：

```sql
DELETE FROM schedules WHERE source_identity = 'reserve:1267942687349145604';
```
