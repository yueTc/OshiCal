# OshiCal Server 0.3.14

本次仅发布服务端，标签为 `server-v0.3.14`。桌面端保持 `0.3.7`。

## 变更

### 长日程图不再报「Image size exceeds limit」

主播卡片上出现过这样一行红字：

```text
图片日程分析失败: 参数校验失败: 图片数据无法解析或超出尺寸限制: Image size exceeds limit
```

`Image size exceeds limit` 在 `image` 0.25.10 里只来自 `LimitErrorKind::DimensionError`，
也就是**宽或高超过 8192**。问题出在缩小的位置：`reader.limits(...)` 把 `8192` 与
`128 MiB` 挂在解码器上，`decode()` 内部先做 header 尺寸校验，而缩到 4096 的 `resize`
排在 `decode()` 之后——所以一张 1080×20000 的日程长图根本走不到缩小那一步，在解码前就被拒了。

即便放开这道门槛，`DynamicImage::resize` 是「等比适配进正方形框」，长图压到 4096
只剩 221 像素宽，字高约 4~5 像素，送给模型也读不出日程。

现在四个常量各自归位：

| 常量 | 原值 | 新值 | 作用 |
| --- | --- | --- | --- |
| `IMAGE_MAX_SIDE` | 4096 | 16384 | 缩放目标；1080×20000 压完是 885×16384，字号约 0.82 倍 |
| `IMAGE_MAX_DECODE_SIDE` | 8192 | 24576 | 解码门槛必须高于缩放目标，否则长图进不来 |
| `IMAGE_MAX_DECODE_ALLOC` | 128 MiB | 256 MiB | 1080×20000 的 RGBA 缓冲 86 MB、4096×8192 需 134 MB |
| `IMAGE_MIN_SHORT_SIDE` | — | 512 | 体积兜底时的短边下限，避免长图被压成一条细线 |

体积降级循环原来是「长边 > 1600 就继续缩」，长图会被一路压扁到 87×1600。现在多一道短边闸门：

```rust
let long_side = image.width().max(image.height());
let short_side = image.width().min(image.height());
if long_side > IMAGE_MIN_SIDE && short_side > IMAGE_MIN_SHORT_SIDE {
    let next_side = ((long_side as u64 * 4 / 5) as u32).max(IMAGE_MIN_SIDE);
    image = image.resize(next_side, next_side, FilterType::Lanczos3);
```

短边缩到 512 仍压不进 7 MiB，就如实报「图片转换为 JPEG 后仍超过模型上传大小限制」，
不再交一张字已经糊掉的图给模型。

`prepare_image_for_llm_with_limit` 只有测试在用，加了 `#[cfg(test)]`；实际逻辑抽成
`prepare_image_for_llm_with_limits(bytes, byte_limit, max_image_side)`，测试可以用 1024
这种小目标值跑，不必在 CI 里生成两万像素高的 PNG。

## 数据库

无变化，不需要迁移。

缓存不受影响：失败的分析本来就不落 `image_analyses`，所以修复后下一次刷新会重新解析那张图，
成功后 `streamers.last_error` 自动清掉。

## 已知限制

- 目标长边 16384 是「可读性与上传体积」的折中。若模型端对大图返回 400，把 `IMAGE_MAX_SIDE`
  改回 8192 即可，其余常量不用动。
- 超过 256 MiB 解码缓冲的图（约 6700 万像素）仍会报 `Memory limit exceeded`，这是刻意的兜底，
  避免后台任务吃满内存。
- 只有 JPEG 有 DCT 级缩放库可用；`image` 0.25.10 的 PNG 路径与 `zune-jpeg` 都没有「解码时缩放」入口，
  所以缩小只能发生在整幅解码之后。

## 验证

- 本机只做源码与格式检查：`rustfmt --check crates/core/src/llm.rs`，通过。
- 按 `AGENTS.md`，Clippy（`-D warnings`）、测试与服务器镜像构建全部由 GitHub Actions 完成。
- 测试调整：`downsamples_image_long_side_to_4096` 改为传参的
  `downsamples_image_long_side_to_target`（原断言跟着常量走，目标一改就失效）；
  新增 `decode_limits_leave_room_for_tall_schedule_images`，钉住「解码门槛 ≥ 缩放目标、
  1080×20000 装得进 alloc、压完宽度 ≥ 800」三条。
