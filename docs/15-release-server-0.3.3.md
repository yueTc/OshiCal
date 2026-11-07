# OshiCal Server 0.3.3

标签 `server-v0.3.3`。桌面端保持 0.3.2，本次只发服务端。

## 新增：服务端编译产物

此前 `server-v*` 只产出 GHCR 镜像与签名清单，没有可直接下载的运行包。0.3.3 起新增
`.github/workflows/server-binary.yml`，在同一个标签上额外编译并附加：

| 平台 | 产物 |
| --- | --- |
| Linux x86_64 | `oshical-server-0.3.3-linux-x86_64.tar.gz` |
| Windows x86_64 | `oshical-server-0.3.3-windows-x86_64.zip` |

- 包内含 `oshical-server`（Windows 为 `.exe`）、`dist/` 前端资源、`.env.example`、`START-HERE.md` 与 `DEPLOYMENT.md`。
- 每个包附带 `.sha256` 校验文件。
- 解压即可运行，不需要 Docker、Rust 或 Node.js。

工作流行为：

1. `meta` 读取 `server/Cargo.toml` 版本并校验标签一致。
2. `build` 在 `ubuntu-latest` 与 `windows-latest` 上分别跑 `npm run build:web` 与 `cargo build --release -p oshical-server --locked`，打成压缩包上传 artifact。
3. `publish` 等 `Server Image` 先把 release 建出来（最多等 30 分钟），再把压缩包与校验和 `gh release upload --clobber` 追加到同一个 release；不会抢先创建 release 从而顶掉镜像清单。

## 沿用 0.3.2 的功能

- 「月相」开关：默认关闭，关闭时不发任何 LAPLACE 请求；开启需确认免责声明与致谢。
- 服务端 6 小时采集循环、`fertility_cache` 6 小时缓存、失败回退旧缓存并标记 `stale`。
- `GET /api/fertility?refresh=`、设置项 `fertility_enabled`。

## 验证

本机只做源码与静态检查。编译、打包与发布全部由 GitHub Actions 执行：

- `.github/workflows/ci.yml`：类型检查、fmt、Clippy、Rust 测试、前端测试与前端构建。
- `.github/workflows/server-image.yml`：GHCR 镜像 `ghcr.io/yuetc/oshical-server:0.3.3` 与签名清单 `latest-server.json`。
- `.github/workflows/server-binary.yml`：两个平台的压缩包。
