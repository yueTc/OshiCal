# OshiCal Server 0.3.9

本次仅发布服务端，标签为 `server-v0.3.9`。桌面端保持 `0.3.5`，不再重复发布。

## 修复生理期接口长时间「读取中」

桌面端生理期徽标会一直停在「读取中…」。排查测试宿主机（华为云节点）后确认：`/api/fertility` 在缓存为空或过期时会在请求路径上同步抓取 LAPLACE，实测 37 秒后才返回 `502 EXTERNAL_ERROR [FERTILITY_UNAVAILABLE]: LAPLACE 返回 HTTP 403`；而桌面端会持续重试该接口，于是界面一直转圈，服务端日志里也堆满 `laplace 直连抓取失败 attempt=1/2/3`。

原因是该机房出口抓 `https://workers.vrp.moe/laplace/fertilities?showOutdated=1` 时被 Cloudflare 托管挑战拦截：`403`、`cf-mitigated: challenge`、`Just a moment...`。换 UA、补 Referer/Origin、降级 HTTP/1.1 都无效，`rustls` 使用内置 webpki 根证书，本地 MITM 代理也无法绕过；兜底 `r.jina.ai` 在该机房不可达。同一地址用 `curl_cffi` 的 Chrome 指纹直连是 200（1639 条）。

本次服务端改动：

- `/api/fertility` **不再在请求路径上抓取**：命中 6 小时缓存直接返回；缓存过期或为空时立即返回现有数据（`stale=true` + 中文 `warning`），并在后台触发一次刷新，界面不再长时间停在「读取中」。
- `?refresh=1`（界面上的「立即刷新」）最多等待 15 秒：抓到就返回新数据，失败或超时回落到缓存。
- 采集改为**单飞**（`AtomicBool` + RAII 守卫）：后台任务、`refresh=1` 与 `runtime.rs` 的 6 小时循环同时只允许一个 LAPLACE 抓取，避免重复请求把上游刷成风控。
- 数据源可配：新增 `OSHICAL_FERTILITY_URL` / `OSHICAL_FERTILITY_READER_URL`，可指向自建中转或镜像；抓取连接超时收紧到 10 秒。
- 后台刷新失败只写 `warn` 日志，不影响接口返回值。

## 兼容性

- 响应结构不变；`stale` / `warning` 会更常见（缓存过期但后台还没刷完时）。
- 桌面端 `0.3.5` 无需改动即可兼容。
- 已有 `fertility_cache` 直接复用，无需迁移或清库。

## 部署提示

- 机房出口能正常抓 LAPLACE 时，什么都不用配。
- 出口被 Cloudflare 挑战时，两种做法都可以：
  1. 在宿主机用浏览器指纹抓取并写入 `fertility_cache`（本次测试宿主机即此方案，`/api/fertility` 只读缓存，5~10ms 返回）；
  2. 把 `OSHICAL_FERTILITY_URL` 指向自建中转（容器访问宿主机用 docker 网桥网关，通常是 `172.17.0.1`），让容器自己采。

## 验证

- 本机只做源码与格式检查（`cargo fmt --all -- --check`），不执行构建、Clippy 或测试。
- Rust 格式、Clippy（`-D warnings`）、单元测试与服务器镜像全部由 GitHub Actions 验证并发布。
