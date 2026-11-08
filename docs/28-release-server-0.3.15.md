# OshiCal Server 0.3.15

本次仅发布服务端，标签为 `server-v0.3.15`。桌面端保持 `0.3.8`。

0.3.14 把「长图解码前就被尺寸上限拒绝」修好了，但把发送给模型的长边放到 16384，
上游直接拒收。0.3.15 收回到长边 4096 并补一道像素总量闸。

## 变更

### 上游拒收超大的图：`400 image_invalid`

0.3.14 上线后，泽音 Melody 那条 8 图动态里最后一张 `03d94193….jpg` 分析失败，网关日志是：

```text
[upstream] chat_stream: upstream 400 image_invalid
body={"code":11135,"msg":"Please start a new conversation, replace the image, and try again."}
```

这张图实测 **1500×9851（约 1477 万像素，7.4 MB）**：

- 旧版本在 `decode()` 的 8192 宽高上限处失败（`Image size exceeds limit`，就是卡片上那行红字）；
- 0.3.14 能解码了，但长边 9851 < 16384，于是**原样**送出去，上游回 400。

同一条动态里另外 7 张（最大 2048×4096，约 839 万像素）在 0.3.13 时都分析成功并落过缓存。
所以「可用」的证据边界是 839 万像素，「被拒」的证据边界是 1477 万像素。

### 发送上限改成两个约束取更紧的

| 常量 | 0.3.14 | 0.3.15 | 说明 |
| --- | --- | --- | --- |
| `IMAGE_MAX_SIDE` | 16384 | 4096 | 沿用长期可用的长边 |
| `IMAGE_MAX_PIXELS` | — | 8,000,000 | 新增总量闸，正好覆盖已验证的 839 万像素 |
| `IMAGE_MAX_DECODE_SIDE` | 24576 | 24576 | 解码门槛保持，长图不再在缩小前被拒 |
| `IMAGE_MAX_DECODE_ALLOC` | 256 MiB | 256 MiB | 同上 |
| `IMAGE_MIN_SHORT_SIDE` | 512 | 512 | 体积兜底时的短边下限 |

缩放逻辑抽出 `fit_within(image, max_side, max_pixels)`：先算长边比例，再算像素比例，
取更小的那个，`resize_exact` 等比缩放，不变形。1500×9851 因此变成 624×4096（约 256 万像素）。

### 代价

长图在 4096 长边下宽度只有原图的 0.42 倍（1500 → 624），字号能读但不如原图。
要真正拿回长图的字，只能纵向切片：把一张长图切成若干 4096 高的片段，同一次请求里带多张图。
那是下一步的事，本次先保证「不报错、上游不拒、能出日程」。

## 数据库

无变化，不需要迁移。失败不落缓存，所以修复后下一次轮询会重新解析那张图。

## 已知限制

- 像素上限 800 万是按「839 万可用 / 1477 万被拒」这条经验边界定的，没有探测出精确阈值；
  上游（腾讯 copilot 转发的 deepseek 视觉模型）只回 `image_invalid`，不给具体原因。
- 长图压到 4096 长边后短边偏小，字小而密的周表仍可能识别不全，需要切片才能彻底解决。
- 超过 256 MiB 解码缓冲的图（约 6700 万像素）仍会报 `Memory limit exceeded`。

## 验证

- 本机只做源码与格式检查：`rustfmt --check crates/core/src/llm.rs` 通过。
- 按 `AGENTS.md`，Clippy（`-D warnings`）、测试与镜像构建全部由 GitHub Actions 完成。
- 线上实测：0.3.14 部署后该图报 `LLM_400`（`image_invalid`），确认是上游尺寸问题而不是解码问题。
- 测试：`downsamples_image_long_side_to_target`、`tall_image_keeps_proportional_width_when_scaled`
  覆盖等比收敛；常量不变量用 `const _: () = assert!(...)` 在编译期校验。
