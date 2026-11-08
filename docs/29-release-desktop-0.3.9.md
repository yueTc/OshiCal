# OshiCal Desktop 0.3.9

本次仅发布桌面端，标签为 `desktop-v0.3.9`。服务端保持 `0.3.15`，不再重复发布。

0.3.8 的桌面包编译的是长边 16384 的那版核心库，长图会被原样送给模型、上游回
`400 image_invalid`。同一处修复已于服务端 `0.3.15` 先行上线，本次桌面包把它带上。

## 变更

### 长图不再被上游拒收

`crates/core/src/llm.rs` 的发送侧上限从「只有长边」改成「长边 + 像素总量」双约束，取更紧的那个等比缩放：

| 常量 | 0.3.8 桌面端 | 0.3.9 | 说明 |
| --- | --- | --- | --- |
| `IMAGE_MAX_SIDE` | 16384 | 4096 | 回到长期可用的长边 |
| `IMAGE_MAX_PIXELS` | — | 8,000,000 | 新增总量闸，覆盖已验证可用的 839 万像素 |
| `IMAGE_MAX_DECODE_SIDE` | 24576 | 24576 | 解码门槛保持，长图不会在缩小前被拒 |
| `IMAGE_MAX_DECODE_ALLOC` | 256 MiB | 256 MiB | 同上 |
| `IMAGE_MIN_SHORT_SIDE` | 512 | 512 | 体积兜底时的短边下限 |

缩放逻辑抽成 `fit_within(image, max_side, max_pixels)`：分别算长边比例与像素比例，取较小者，
再 `resize_exact` 等比缩放，不变形。

以触发问题的那张日程长图为例：**1500×9851（约 1477 万像素）** ——

- 0.3.8：长边 9851 < 16384，原样送出，上游 `400 image_invalid`，卡片显示分析失败；
- 0.3.9：像素闸生效，缩到 **624×4096（约 256 万像素）**，上传成功并可解析出日程。

## 用户可见影响

- 动态里附超长图（周表、长截图）时不再出现「图片分析失败 / LLM_400」，日程能正常落地。
- 失败不落缓存，升级后下一次轮询会重新解析之前失败的那张图，无需手动重试。
- 代价与 0.3.15 相同：长图在 4096 长边下宽度只有原图约 0.42 倍，字小而密的周表仍可能识别不全；
  彻底解决要靠纵向切片（同一次请求带多张 4096 高的片段），留待后续版本。

## 兼容性

- 只改核心库的图像预处理，IPC 命令、HTTP 接口、数据模型、数据库都没有变化，本地库与 0.3.8 完全兼容。
- 编译期断言维持 `IMAGE_MAX_DECODE_SIDE >= IMAGE_MAX_SIDE`（24576 ≥ 4096）与
  `IMAGE_MIN_SHORT_SIDE < IMAGE_MIN_SIDE`（512 < 1600），常量配错会直接编译失败。

## 验证

- 按 `AGENTS.md`，本工作区不做任何编译：格式检查、Clippy（`-D warnings`）、前端与 Rust 测试、
  NSIS 安装包、更新签名与 `latest.json` 全部由 GitHub Actions（`.github/workflows/release.yml`）完成。
- 该修复的源码级验证见 `docs/28-release-server-0.3.15.md`：`downsamples_image_long_side_to_target`、
  `tall_image_keeps_proportional_width_when_scaled` 覆盖等比收敛，常量不变量用 `const _: () = assert!(...)`
  在编译期校验；线上实测确认 0.3.14/0.3.8 的 16384 行为会触发 `image_invalid`。

## 发布资产

`OshiCal_0.3.9_x64-setup.exe`、`.sig`、`latest.json`（由 CI 生成）。
