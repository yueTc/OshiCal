# 验收与测试矩阵

## 1. 核心单元测试

| 编号 | 场景 | 预期 |
| --- | --- | --- |
| P-01 | “10月30日 20:00 直播” | 2026-10-30 20:00 +08:00 |
| P-02 | “今晚 8 点开播” | 当天 20:00 |
| P-03 | “明天 21:30-23:00” | 次日正确起止时间 |
| P-04 | “周日晚 7 点” | 当前自然周周日 19:00 |
| P-05 | “20:00~22:00” | 当天或未来最近日期 |
| P-06 | 直播预约卡片 | 标题、房间号、标题文本正确 |
| P-07 | 模型 JSON 合法 | 结构化和时间校验通过 |
| P-08 | 同一图片两个主播 | 模型调用次数为 1 |
| P-09 | 同主播两来源相差 5 分钟 | 合并为一条并保留来源 |
| P-10 | 删除自动日程后重复抓取同来源 | 不恢复 |
| P-11 | 删除后主播发布新动态 | 允许重新生成 |
| P-12 | 手动修改后来源更新 | 保留手动值并标记来源变化 |
| P-13 | 取消预约 | 未来隐藏、历史保留 |
| P-14 | 关闭日程订阅 | 未来自动日程隐藏 |
| P-15 | 重新开启日程订阅 | 恢复未删除日程 |

## 2. 解析反例

| 编号 | 输入 | 预期 |
| --- | --- | --- |
| N-01 | “今天想吃火锅” | 无时间，忽略 |
| N-02 | “上周一 20:00” | 已过去，丢弃 |
| N-03 | “24:90 直播” | 非法时间，丢弃 |
| N-04 | “下个月 20:00” | 不支持的日期，丢弃 |
| N-05 | 空标题图片事件 | 丢弃 |
| N-06 | 非 hdslb 图片 URL | 拒绝下载 |
| N-07 | 只有日期没有时间 | 丢弃 |
| N-08 | 服务器离线重复同步同一 mutation | 只应用一次 |
| N-09 | 手动编辑与服务器冲突且未选择策略 | 停止同步并展示冲突摘要 |

## 3. API 与 WebSocket

| 场景 | 证据 |
| --- | --- |
| 健康检查 | 200 + `{"ok":true}` |
| 无 Token 访问订阅 | 401 |
| 错误 Token 访问订阅 | 401 |
| 添加订阅幂等 | 两次相同 MID 仅一条 |
| 打开日程 | 返回直播 URL |
| WebSocket 首帧错误 Token | 服务端关闭连接 |
| WebSocket 首帧正确 Token | 返回 auth_ok 与 seq |
| 状态变化 | 1 秒内收到 live_status |
| 断线重连 | events after 补拉无重复 |

## 4. GUI 真人验证

1. 启动服务端，打开 Web UI，输入 Token。
2. 搜索“嘉然”，添加订阅。
3. 设置官号，刷新，查看日程。
4. 切换本地模式，重复搜索和日程查看。
5. 模拟开播状态变化，确认 Windows 通知出现。
6. 点击通知，确认默认浏览器打开 B 站直播间。
7. 关闭某主播的日程订阅，再刷新，确认不新增该主播日程。
8. 关闭开播通知，确认状态变化不产生通知。
9. 断开服务端，确认 UI 显示离线；恢复后自动重连。

## 5. 安全测试

- 注入“忽略之前指令”的动态文本。
- 模型返回 `<script>alert(1)</script>`。
- 图片 URL 指向 localhost、IPv6 回环和非 hdslb 域名。
- 超长标题、超长文本、非法时间。
- 远程绑定且无 Token 启动。
- 日志和数据库全文搜索 API Key、Cookie、Token 标志。

## 6. 构建证据

- `cargo fmt --all -- --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `npm run build:web`
- `npm run build:desktop`
- 服务端真实启动与 HTTP/WebSocket 响应
- GUI 截图与操作步骤



## 7. 第二轮功能验收

| 场景 | 操作 | 预期证据 |
| --- | --- | --- |
| 7 天窗口 | 从今天开始查看，连续点击后移 1 天 | 日期范围始终为起点 + 6 天 |
| 视图切换 | 时间线切到网格再切回 | 日程集合一致，偏好持久化 |
| 手动增改删 | 本地、服务器在线、服务器离线分别操作 | 在线即时生效，离线进入待同步 |
| 服务器导入 | 客户端与服务器存在冲突 | 展示摘要并支持三种统一策略 |
| 导入不改服务器 | 执行全部冲突策略 | 服务器数据库内容不变 |
| 托盘 | 关闭窗口后等待开播推送 | 进程仍在，通知正常 |
| 自启 | 开启后重启系统 | 默认关闭；开启后应用启动 |
| 官号监控 | 官号开播 | 不产生通知 |
| 同时开播 | 两个主播同时开播 | 两条独立通知，分别跳转 |
| 启动已直播 | 启动或重连时已直播 | 不补发通知 |
| 提前提醒 | 日程开始前等待 | 无通知 |
| 预约取消 | 模拟取消状态 | 未来视图隐藏，历史可查 |
| 右键与长按 | 桌面右键、模拟移动长按 | 菜单包含编辑/删除/查看来源 |
| Docker | 重启容器 | 挂载数据库数据仍存在 |

## 8. 实际验收记录（2026-10-26）

| 项目 | 结果 | 证据 |
| --- | --- | --- |
| Rust 全量测试 | 通过 | 37 passed，0 failed，1 ignored；server API 4 passed |
| Rust clippy | 通过 | workspace all targets，无警告 |
| 前端生产构建 | 通过 | Vite build，295.06 KiB JS |
| npm audit | 通过 | 0 vulnerabilities |
| B 站真实搜索/直播 | 通过 | ignored smoke test 显式运行成功 |
| 官号同图共享模型 | 通过 | 单元测试调用次数为 1 |
| 10 分钟日程合并 | 通过 | 单元测试两条来源合并为一条 |
| 浏览器 Web UI | 通过 | 登录、搜索、订阅、手动日程、自动预约、网格、右键 |
| Tauri 本地 UI | 通过 | 搜索、订阅、设置页、托盘驻留 |
| release EXE | 通过 | 内嵌前端启动并显示主界面 |
| 本轮便携版构建 | 通过 | desktop 与 server release 构建；复制产物 SHA-256 与 target/release 一致 |
| Release Web UI 同步 | 通过 | HTML 引用当前 JS/CSS 哈希文件 |
| NSIS 安装程序 | 已构建，未安装验证 | `release/windows/oshical-0.1.0-x64-setup.exe`，约 4.91 MiB，发布副本 SHA-256 与构建产物一致 |
| MSI 安装包 | 未配置 | 当前 Tauri bundle target 仅配置 NSIS |
| Docker | 通过（历史验收） | 已在隔离 Linux Docker 环境完成部署与数据持久化验收 |
| 通知自动点击 | 部分验证 | 测试通知调用无错误，点击需人工确认 |

## 9. 实际验收增量（2026-10-27）

| 项目 | 本轮结果 | 证据 |
| --- | --- | --- |
| Rust workspace 测试 | 通过 | 核心 42、桌面 3、服务端运行时 1、API 4，共 50 passed；2 ignored |
| 数据库一致性备份/恢复 | 通过 | `VACUUM INTO` 快照可重开并恢复订阅/日程；已有备份目标拒绝覆盖 |
| 备份改动后的 Release 重建 | 通过 | 桌面端、服务端、NSIS 构建成功；发布副本 SHA-256 与构建源一致；新安装包 7-Zip 完整性通过 |
| B 站真实接口 | 通过 | 显式运行 `real_bilibili_smoke`，1 passed |
| 格式与静态检查 | 通过 | fmt、Clippy all targets 无警告 |
| 前端构建与依赖审计 | 通过 | Vite 构建成功；npm audit 0 漏洞 |
| 敏感字符串扫描 | 通过（人工复核） | 2 个预期命中文件为 Cookie 字段名/测试夹具；未发现真实凭据；不替代专业 secret scanner |
| 发布 Web UI 一致性 | 通过 | `dist/` 与 `release/server/dist/` 的 HTML、JS、CSS 一致 |
| NSIS 归档完整性 | 通过（静态） | 7-Zip 24.07 `t`：Everything is Ok；NSIS-3 Unicode/LZMA |
| Windows Authenticode | 未通过/未配置 | 桌面端、服务端和 NSIS 安装包均为 `NotSigned` |
| RustSec 依赖审计 | 工具不可用 | `cargo audit`、`cargo deny` 未安装；本轮未安装 |
| Docker 镜像构建 | 通过 | 隔离 Linux Docker 环境，Ubuntu x86_64 / Docker 29.1.3 / Compose 2.40.3；镜像约 37.58 MB，无 OOM |
| Docker 健康与鉴权 | 通过 | `/api/health` 返回 ok；无 Token 401、有 Token 200 |
| Docker HTTP 与 WebSocket | 通过 | 创建订阅/日程成功；错误 Token WS 被拒绝；正确 Token 收到 `schedule_changed`，事件序号 1 |
| WebSocket 推送延迟 | 通过 | HTTP 更新到事件送达约 354.3 ms，低于 1 秒目标 |
| Docker 数据持久化 | 通过 | `down/up` 后容器 ID 变化，订阅与日程仍各 1 条；SQLite `integrity_check=ok` |
| Docker Web UI | 通过 | `/`、`index-C2-IHiq1.js`、`index-BEQrl6Dj.css` 均返回 200 |
| 公网 HTTPS | 通过 | 自有 HTTPS 测试入口：health 200、root 200、无 Token 401、TLS verify result 0 |
| 公网 WSS | 通过 | 错误 Token 收到 `REMOTE_UNAUTHORIZED`；正确 Token 鉴权成功并收到 `schedule_changed`，延迟约 40.6 ms |
| 公网 Web UI/CSP | 通过 | 真实浏览器加载 HTTPS 测试页面，正常渲染“连接服务器”界面，无脚本/样式阻断 |
| HTTPS 安全响应头 | 通过 | CSP、nosniff、DENY frame、no-referrer、Permissions-Policy 均生效；Nginx 版本号隐藏 |
| WebSocket 子路径回归 | 通过 | 桌面端保留服务器 URL 路径前缀，`/oshical/api/ws` 回归通过 |
| B 站搜索初始化重试 | 通过 | `finger/spi` 与 WBI 初始化使用最多 3 次网络重试；公网搜索“嘉然”返回 20 条并命中 MID 672328094 |
| B 站二维码 API | 通过 | 真实生成二维码并轮询到 waiting/scanned/expired 状态；Cookie 解析与域名白名单回归通过 |
| 扫码登录 UI | 通过 | 隔离桌面实例实际显示 B 站二维码、等待状态和重新生成按钮 |
| WebView 登录窗口 | 通过 | 隔离桌面实例实际加载 `https://passport.bilibili.com/login`，支持扫码、密码和短信登录 |
| WebView Cookie 过滤 | 通过 | 仅接受 B 站域名 Cookie 且必须包含 SESSDATA；其他域 Cookie 被忽略 |
| 扫码回调域名兼容 | 通过 | `passport.biligame.com/crossDomain` 白名单回归通过；B 站/Cookie 字段提取正确 |
| 主播库横向滚动 | 通过 | 隔离桌面实例空状态和搜索结果布局不再出现页面横向滚动条 |
| 退出登录命令 | 通过（编译/UI） | 未配置 Cookie 时按钮禁用；确认后删除本机凭据并广播状态，不修改服务端环境变量 |
| 扫码 Cookie 多来源 | 通过 | 回调 URL、嵌套 `gourl`、`Set-Cookie` 均可提取；缺失 SESSDATA 时仅显示参数名 |
| 查看来源跳转 | 通过 | HTTPS 动态页/作品页/直播间白名单回归通过，非 B 站域名拒绝 |
| 官号多人文本日程 | 通过 | 大模型返回甲、乙两个事件并归到不同主播，标题去除主播名 |
| 动态文字模型 | 通过 | HTTP Mock 验证文字 Chat Completions 请求、多主播 JSON 解析与文字摘要缓存键 |
| 无正则回退 | 通过 | 动态自由文本路径已移除 `parse_text_schedules`；仅预约卡片保留结构化时间解析，且只作模型不可用时的兜底 |
| 页面横向滚动 | 通过 | 隔离桌面实例中主播库与设置页均无页面级横向滚动条 |
| 官号多人图片日程 | 通过 | 同一图片只调用一次模型，多事件分别按主播名称/别名和置信度归属 |
| 真实 B 站登录完成 | 未验证 | 无用户账号凭据，未执行登录成功后的 Cookie 自动保存端到端操作 |
| 本机独立服务端 EXE | 阻塞（非 Docker） | 启动被执行策略拒绝；未重试或绕过，Docker 验收不受影响 |
