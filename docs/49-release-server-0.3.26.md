# 服务端 0.3.26

## 症状

九宫格动态（一条动态带 9 张图）在线上稳定分析失败：

```
WARN oshical_core::service: post analysis failed error=网络请求失败: error sending request for url (…/v1/chat/completions)
```

每条都要等满 90 秒超时，一次全量刷新里这类动态占多数，把刷新拖到十几分钟。

## 根因

两类图片用例被混在同一个压缩口径里：

- 单图动态：`prepare_image_for_llm` 把长边压到 ≤4096、字节压到 ≤7 MiB，没问题。
- 九宫格动态：每张图各自压到 7 MiB 上限，9 张合计 **31 MB 图片 / 41 MB 请求体**。

实测同一批图打网关：

| 压缩口径 | 请求体 | 结果 |
| --- | --- | --- |
| 长边 4096 / q90（旧） | 41 MB | HTTP 413 / 上传途中连接被断，客户端表现为 90 秒超时 |
| 长边 1024 / q80 | 2.2 MB | 15.4 秒返回 |
| 长边 768 / q75 | 1.1 MB | 10.5 秒返回 |

也就是说旧逻辑对多图动态必然失败，且失败表现是「超时」，看不出真实原因是请求体过大。

## 改动

1. `crates/core/src/llm.rs` 新增 `const MODEL_REQUEST_IMAGE_BUDGET_BYTES: usize = 3 * 1024 * 1024;`
   与 `const MODEL_MIN_IMAGE_BYTES: usize = 160 * 1024;`。
2. `analyze_post` 按图片数量分摊预算：
   `(MODEL_REQUEST_IMAGE_BUDGET_BYTES / 图片数).clamp(MODEL_MIN_IMAGE_BYTES, IMAGE_UPLOAD_JPEG_BYTES)`，
   再交给 `load_image_payload_with_budget`。
3. 新增 `max_side_for_budget`：预算 ≤192 KiB → 长边 768，≤512 KiB → 1024，否则沿用 4096。
4. `encode_image_data_url_with_budget` 用这份预算与长边做压缩，仍保留 10 MiB 单图硬闸。
5. 传输层重试加 `!error.is_timeout()`：超时的请求不再重试两次（旧逻辑会把一条动态从 90 秒拖到 280 秒）。

## 测试

- 新增 `many_images_are_compressed_into_the_request_budget`：9 张 1600×1600 噪声图按预算压缩后，
  单张 ≤ 预算、base64 总量 ≤ 1.5 倍预算。
- 新增 `tighter_budgets_use_smaller_long_edges`。
- `cargo test -p oshical-core --locked` → 208 passed / 0 failed / 6 ignored
- `cargo test --workspace --locked` 全绿；`cargo clippy --workspace --all-targets` 无告警；`cargo fmt --all -- --check` 通过

## 升级

```bash
docker pull ghcr.io/yuetc/oshical-server:0.3.26
docker stop oshical-test && docker rename oshical-test oshical-test-prev-0323
docker run -d --name oshical-test ... ghcr.io/yuetc/oshical-server:0.3.26
```

## 已知取舍

- 多图动态的图会压得更小（长边 768），九宫格里的细小文字可能更难读；换来的是这批动态从「必然失败」
  变成「十几秒出结果」。单图动态不受影响，仍按 4096 长边压缩。
- 预算按图片数量平摊，没有按内容重要性加权；如果以后需要保留某几张原图，得在采集侧标记主图。
