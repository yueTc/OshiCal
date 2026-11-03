# 推历 OshiCal

B 站虚拟主播日程与开播通知工具。支持 Windows 桌面端、本地抓取、自托管服务器、Web 管理界面和 Windows 通知。

> 内部标识已统一为 `OshiCal` / `oshical`。当前版本不迁移旧版数据；服务器升级采用停止旧进程、删除旧数据后全新安装。

## 功能

- B 站昵称、UID、`@昵称`、空间链接搜索与订阅；输入自动防抖，结果去重排序并缓存 5 分钟。
- 搜索栏有内容时内嵌清空按钮，点击或按 Esc 可清空文字、结果和错误提示。
- B 站登录窗口自动获取 Cookie，或使用手机扫码登录。
- B 站退出登录：确认后删除本机保存的 Cookie，不影响服务端环境变量。
- 官号多人日程会按每个主播的局部上下文拆分，并分别归属。
- 多人团播会合并为一条共享日程，保存全部参与者（含未订阅人员）；A-SOUL 展开为乃琳、嘉然、贝拉，并识别实际直播间所属账号，不按参与者重复建日程。
- 同一场团播即使先由图片、后由文字识别、由预约卡和文字分别发布，或归属到不同主账号，也会按开始时间、参与者重叠和直播间归属合并；预约标题优先并去掉 B 站“直播预约：”前缀。
- A-SOUL 成员即使只订阅其中一人，也会同时抓取个人账号和 A-SOUL_Official；主播库会显示“自动”官号来源。
- 动态按发布时间从旧到新处理，新公告修改旧日程；旧公告不能反向覆盖新公告。
- 删除主播会直接删除她的日程、官号关联和来源抑制记录。
- 个人账号动态、直播预约卡片和图片日程解析；视频预约卡片不会被当作直播日程。
- 有日程图片的动态会把图片与完整文字描述放入同一次多模态模型请求；图片提供时间/主题，文字补齐直播间归属和例外说明。无图片动态才单独走文字模型。
- 所有动态文字与图文模型请求都会附带该条动态的发布时间，用于解析“今天、明天、本周”等相对日期。
- 同一图片与文字组合只调用一次模型；缓存键同时包含图片摘要、文字摘要、模型和 Prompt 版本。
- 发给模型的图片会统一转为 JPEG，透明背景铺白，长边最多 4096 像素，并压缩到 base64 图片数据低于 10 MiB；源图最大允许 32 MiB。
- 动态文字和图片中的明确取消、停播、延期或改期信息会按“主播 + 时间 + 标题”匹配并删除原日程；旧来源版本会被抑制，避免下次刷新复活。
- B 站直播预约卡片必须包含明确日期或今天/明天等相对日期，支持 `10月30日`、`10/30`、`10.30`、`10-30` 与 ISO 日期；只有星期几或只有时间时不再自动生成预约日程。视频预约（`stype = 1`）始终跳过，并清理旧版本已误入日程的记录。
- 带预约卡片的动态不再调用文字或图片模型；直播预约去掉 B 站自带的“直播预约：”前缀后沿用卡片标题，预约与同一动态的正文不会重复计入日程。
- 设置页可用固定日历示例图测试模型连通性、端到端延迟和识别成功率。
- 设置页支持本机 HTTP 代理端口；本地模式由桌面端使用，服务器模式由服务端使用，并可通过 Google 请求测试代理延迟。
- 日程模型可从 OpenAI-Compatible `/models` 接口拉取并搜索选择，仍允许手动输入模型 ID。
- 主播库支持单主播重新拉取，并通过真实阶段进度条展示动态拉取、文字/图片识别和入库状态。
- 主播库卡片和左侧“我的主播”均支持进入直播间；左侧右键菜单还支持刷新或删除，删除前会二次确认。
- 应用启动后直播状态静默刷新完成会主动同步界面，避免残留绿点或漏显示正在直播的主播。
- 左侧栏固定显示，可拖拽调整宽度并持久化；窄屏自动切换为紧凑导航。
- 日程表：从所选日期起连续 7 天、每次移动 1 天。
- 时间线与日历网格两种视图。
- 魔法期页：实现保留，但因 LAPLACE 正式接口尚未接入，当前暂时隐藏入口。
- 桌面右键、移动端长按编辑或删除日程。
- 本地 SQLite 与服务端 SQLite 双模式。
- 服务器 HTTP API + WebSocket 推送。
- Windows 开播通知和默认浏览器直播间跳转。
- 系统托盘、开机自启、手动日程、离线同步队列。
- 直播数据统计：设置中开启后可按主播订阅；直播期间只是接收公开文本事件，原始弹幕和礼物不落盘。
- 日程右键可查看对应场次数据：时长、营收估算、付费人数、峰值人气、互动人数和弹幕数。
- 服务器 Docker 部署文件。

## 技术栈

- Rust 1.99
- Tauri 2
- Axum 0.8
- SQLite / SQLx
- React 19 / TypeScript / Vite

## 当前交付物

- Windows 便携程序：`release/windows/OshiCal.exe`
- Windows NSIS 安装程序：`release/windows/OshiCal-0.1.0-x64-setup.exe`
- 服务端程序：`release/server/oshical-server.exe`
- 服务端静态界面：`release/server/dist/`
- Docker：`Dockerfile`、`compose.yaml`

Windows 便携程序无需 Node.js，直接双击运行。首次运行会在应用数据目录创建 SQLite 数据库。

## 本地开发

```powershell
npm install
npm run dev:desktop
```

服务端开发：

```powershell
$env:OSHICAL_API_TOKEN = "dev-token"
$env:OSHICAL_WEB_DIR = "./dist"
npm run build:web
cargo run -p oshical-server
```

打开 `http://127.0.0.1:8787`，输入 Bearer Token。

## 构建

```powershell
npm run build:web
cargo build --release -p oshical-server
cargo build --release -p oshical-desktop
```

安装包构建：

```powershell
npx tauri build --bundles nsis
```

NSIS 安装程序已生成于 `release/windows/OshiCal-0.1.0-x64-setup.exe`。本次构建未在本机执行安装流程；MSI 未配置。

## GitHub Release 与自动更新

Windows 桌面端使用 Tauri 2 官方 Updater 从 GitHub Releases 检查并安装签名更新。更新会下载完整 NSIS 安装包，安装完成后自动退出并重启；不会直接替换内嵌前端资源。

首次发布前，在 GitHub 仓库配置以下 Secrets：

```text
TAURI_SIGNING_PRIVATE_KEY
TAURI_SIGNING_PRIVATE_KEY_PASSWORD
```

发布新版本：

```powershell
node scripts/set-version.mjs 0.1.1
git add -A
git commit -m "release: v0.1.1"
git tag v0.1.1
git push origin main --tags
```

推送 `v*` 标签后，`.github/workflows/release.yml` 会运行测试、构建 Windows 安装包、生成更新签名和 `latest.json`，并创建 Draft Release。人工检查资产后再发布 Release。

客户端更新地址：

```text
https://github.com/yueTc/OshiCal/releases/latest/download/latest.json
```

详细设计、验收和回滚流程见 `docs/11-github-release-and-updater-plan.md`。
## 服务器配置

复制 `.env.example` 并设置：

```dotenv
OSHICAL_BIND=0.0.0.0:8787
OSHICAL_API_TOKEN=至少32位随机令牌
OSHICAL_DB_PATH=./data/oshical.db
OSHICAL_WEB_DIR=./dist
OSHICAL_LLM_BASE_URL=https://api.openai.com/v1
OSHICAL_LLM_MODEL=你的文字/视觉模型名
OSHICAL_LLM_API_KEY=模型密钥
OSHICAL_SECRETS_KEY=模型密钥落库加密主密钥（设置后不要更改）
OSHICAL_PROXY_PORT=7890
```

密钥和 Cookie 不会返回前端。

## Docker

```powershell
$env:OSHICAL_API_TOKEN = "至少32位随机令牌"
docker compose up -d --build
```

详细说明见 `docs/06-deployment.md`。

部署时请使用自有域名、API Token 和 HTTPS 证书；不要把真实 `.env`、Token、服务器路径或内部测试数据提交到仓库。

## 验证

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
npm run build:web
npm audit --audit-level=high
```

2026-10-29 开播状态同步修复回归：核心 75 项、桌面 3 项、服务端运行时 1 项、API 7 项，共 86 项通过，6 项真实网络测试默认忽略；fmt、Clippy、前端生产构建通过。隔离桌面实例预置“缓存为直播但实际未开播”和“缓存为未开播但实际直播”两位主播，启动后均纠正为真实状态，并写入 `live_status_snapshot` 事件。

2026-10-29 主播库直播间入口：主播卡片新增直播间跳转图标，直播中显示绿色高亮，未配置房间地址时禁用；复用现有安全外链打开逻辑。TypeScript、Vite 生产构建、Rust fmt、Clippy 和发布产物重建通过。

2026-10-29 Cookie 提示自动关闭修复：B 站 Cookie 自动保存事件原先直接写入 toast 状态，绕过 5.5 秒计时，导致右下角提示常驻；现统一通过 `showToast` 自动关闭，并同步修复导入完成提示。TypeScript、Vite 生产构建、Rust fmt、Clippy 和发布产物重建通过。

当前结果（2026-10-28 本轮重跑）：核心 74 项、桌面 3 项、服务端运行时 1 项、API 7 项，共 85 项测试通过，6 项真实网络测试默认忽略；B 站搜索、直播状态和二维码生成烟雾测试通过；真实 B 站直播统计采集烟雾测试通过，15 秒内成功接收弹幕并写入内存聚合；fmt、Clippy、前端生产构建与 npm audit（0 漏洞）通过。固定日历示例图的模型测试接口已通过本地 OpenAI-Compatible Mock 完成 UI 与服务端端到端验收，并显示 5/5 匹配、100% 成功率和真实请求延迟。代理连通性测试、模型列表拉取、筛选和回填已通过验收；直播数据全局开关、主播级订阅开关、日程右键菜单和场次数据弹窗已通过隔离数据库的真实桌窗验收；侧栏键盘调整宽度与本地持久化已验证。模型取消事件已覆盖“匹配唯一日程 → 删除 → 抑制旧来源版本”回归；预约卡片仅星期几时拒绝入库；多人团播已覆盖 A-SOUL 三人展开、多订阅参与者、外部嘉宾、实际直播间账号和单条去重入库；新旧公告覆盖顺序及删除主播级联清理已加入回归。单主播重新拉取已用真实 B 站官号来源触发，WebSocket 收到单调递增的阶段进度并最终入库。便携桌面端、NSIS 安装程序与服务端 Release 程序已构建，发布副本哈希与构建产物一致。隔离 Linux Docker 环境完成公网健康检查、搜索、鉴权、HTTP、WebSocket、Web UI 和持久化验收，公网 WSS 推送延迟约 40.6 ms；本次新增功能已生成 Docker 源码包，尚未覆盖远端容器。安装流程、通知点击与真实供应商模型验证限制见 `docs/07-known-issues.md`。

真实 B 站网络烟雾测试：

```powershell
cargo test -p oshical-core real_bilibili_smoke -- --ignored --nocapture
```

## 安全

- 桌面端模型密钥与 B 站 Cookie 保存到 Windows 凭据管理器。
- B 站登录窗口只允许 B 站域名；Cookie 不回传前端，扫码回调 Cookie 经过白名单字段和长度校验。
- 服务端只从环境变量读取密钥。
- 服务端非回环绑定必须配置 API Token。
- 图片只允许从 `hdslb.com` 下载。
- 直播间跳转只允许 `https://live.bilibili.com/`。
- 日程模型输出经过长度、时间、URL、主播归属和置信度校验后才入库。

## 已知限制

- NSIS 安装包已构建，尚未在本机执行安装/卸载验证；MSI 未配置。
- Windows 桌面端、服务端和 NSIS 安装包当前未做代码签名，分发时可能触发 SmartScreen 或安全软件提示；正式外部分发前应配置可信代码签名证书。
- Docker 已在隔离 Linux 环境完成部署、HTTPS/WSS 和数据持久化验收；正式生产仍需绑定正式域名并配置证书自动续期。
- B 站扫码和内嵌登录窗口已实际打开与显示；由于没有用户 B 站账号凭据，尚未完成一次真实登录后的自动 Cookie 保存端到端验证。
- Windows 通知点击由 Tauri Notification 插件报告；Windows 在 Toast 进入操作中心后可能不再上报点击。
- 图片模型需要用户自行配置；没有有效模型密钥时图片分析不会运行。
- 设置页的模型自检使用仓库内置固定日历图；真实供应商准确率仍取决于用户选择的视觉模型和账号额度。
- B 站动态接口存在风控，必要时在设置中配置可选 SESSDATA。
- 服务器为单用户模式，不支持多用户注册。
- 直播“营收/付费人数”来自公开礼物、SC 和舰长事件估算，不含平台分成、活动折扣或盲盒特殊规则；粉丝侧无法获得官方“高能”指标。

## 文档

- `docs/00-project-brief.md`
- `docs/01-product-spec.md`
- `docs/02-architecture.md`
- `docs/03-ui-spec.md`
- `docs/04-plan.md`
- `docs/05-acceptance.md`
- `docs/06-deployment.md`
- `docs/07-known-issues.md`
- `docs/08-client-server-migration-plan.md`
- `docs/09-server-authoritative-multi-device-sync.md`
- `docs/10-multi-device-sync-implementation-plan.md`
