# 实施计划与 PLAN-GATE

## 状态

- 当前阶段：核心功能与 Wave 1–5 已实现并通过本轮回归；Wave 6 交付验证仍有明确限制
- 默认循环轮次：3
- 单 Wave 安全最大轮次：6
- 变更规模：大项目，预计 30 个以上文件、2000 行以上代码
- 当前基线（2026-10-26）：Rust 1.99.1；37 项测试通过、1 项网络测试忽略；Clippy、前端构建、npm 审计通过；便携桌面端与服务端已重新构建并同步到 release/

## PLAN-GATE

### A 池：无条件核心项

- [x] A1 需求到步骤映射完整：产品契约覆盖账号、官号、文本/预约/图片日程、双模式导入、离线同步、手动编辑、通知、托盘、7 天视图和推送。
- [x] A2 步骤含出口校验：每个 Wave 定义编译、测试、接口或 GUI 证据。
- [x] A3 数据与外部依赖标注：SQLite 表、B 站接口、WBI、视觉模型、Tauri、Axum 已列出。
- [x] A4 风险及缓解：B 站风控、模型误归属、通知点击、密钥泄露、SSRF 均有控制。
- [x] A5 验证方式定义：命令输出、测试报告、HTTP 响应、WebSocket 消息、GUI 截图。
- [x] A6 回滚机制：源码未提交前按文件恢复；数据库发布前复制文件；迁移向后兼容。
- [x] A7 批量范围确认：桌面端、服务端、核心、React、测试、文档共 6 个边界。
- [x] A8 安全触发面：外部输入、SQLite、HTTP、WebSocket、密钥、模型输出已评估。
- [x] A9 人工改动遍历：项目为空，无既有用户改动会被覆盖。
- [x] A10 中断恢复：每 Wave 写 `.ai-memory/handoff.md`，按 Wave 继续。

### B 池：条件触发项

- [x] B1 数据库迁移回滚：仅新增表/列；发布前复制 DB；失败恢复备份。
- [x] B2 新依赖审查：只选用 MIT/Apache 生态主流 crates 和 npm 包，构建时用 cargo/npm 审计。
- [x] B3 并发与幂等：SQLite 唯一键、刷新锁、事件序号、WebSocket 重连补拉。
- [x] B4 性能路径：20 个主播、20 秒轮询，按 5 个一批错峰请求；记录请求耗时。
- [x] B5 外部接口：B 站公开接口可选 Cookie，模型接口可配置。
- [x] B6 测试集：解析器单测、缓存幂等测试、API 集成测试、WebSocket 测试、GUI 验证。
- [x] B7 批处理：动态刷新按订阅分批，单源失败隔离。
- [x] B8 不可逆操作：删除订阅/日程在 GUI 二次确认；数据库发布前备份。
- [x] B9 多文件影响：空项目，依赖方向已固定，后续用全仓 grep 复核。
- [x] B10 CLI/脚本参数：不使用硬编码绝对路径，使用环境变量与平台默认目录。
- [x] B11 方案回读：文档写入后重新读取检查编码与截断。

### C 池：规模触发项

- [x] C1 每 5 个文件或每个 Wave 结束写检查点。
- [x] C2 按 Wave 链式执行，Wave 结束更新 handoff。

## 规划自审

1. 占位符扫描：文档中不保留未完成标记或未定义变量。用户配置项通过设置页或环境变量提供。
2. 类型/签名一致性：Rust crate、Tauri command、HTTP path、SQLite 字段在编码前统一命名。
3. 需求到步骤映射：每条 In scope 功能至少落入一个 Wave 验收项。
4. 引用点实码 trace：当前为空仓库，外部接口结构已通过 B 站实测与 API 文档确认；实现后逐点 grep 复核。

## 失败模式分档

| 场景 | 处置 | 产出通道 |
| --- | --- | --- |
| 搜索为空 | 放行 + 标记 | 空列表与提示 |
| 搜索接口风控 | 拒绝 | `BILI_RISK_CONTROL` |
| 单个官号刷新失败 | 放行 + 标记 | 该源 `last_error`，其他源继续 |
| 图片模型未配置 | 放行 + 标记 | 跳过图片，设置页提示 |
| 模型超时 | 拒绝本次分析 | 错误摘要，允许手动重试 |
| 模型输出非法 | 拒绝 | `LLM_BAD_OUTPUT`，不入库 |
| 主播归属不唯一 | 拒绝 | 直接丢弃 |
| WebSocket 鉴权失败 | 拒绝 | 关闭连接并要求更新 Token |
| WebSocket 断线 | 放行 + 标记 | 离线状态并补拉事件 |

## 二阶效应

- 轮询频率提高 ⇒ B 站请求数上升 ⇒ 触发风控。控制：分批、默认 20 秒、可调高。
- 共享模型分析 ⇒ 缓存键错误会跨主播污染。控制：缓存键包含图片摘要、模型和 Prompt 版本。
- 自动主播别名 ⇒ 错误归属影响日程可信度。控制：高阈值、唯一匹配、低置信度丢弃。
- WebSocket 重连 ⇒ 重复通知。控制：事件序号去重 + 主播开播状态通知锁。

## 歧义清单

| 歧义 | 决定 |
| --- | --- |
| 无日期周表归属 | 当前自然周 |
| 当前周日期已过去 | 直接丢弃 |
| 多个主播共享官号图片 | 只调用一次模型 |
| 动态刷新默认周期 | 5 分钟 |
| 日程默认窗口 | 从今天起连续 7 天 |
| 前后移动步长 | 每次 1 天 |
| 视图形式 | 日历网格与时间线可选 |
| 自动日程删除 | 只屏蔽当前来源版本 |
| 关闭日程订阅 | 隐藏未来自动日程 |
| 取消预约 | 标记取消并隐藏未来显示 |
| 启动时已开播 | 不补发通知 |
| 开播前提醒 | 第一版不做 |
| 低置信度归属 | 直接丢弃 |
| 服务器用户模型 | 单用户 |
| 轮询默认值 | 20 秒 |
| 密钥保存 | Windows Credential Manager / 服务端环境变量 |

## Wave 计划

### Wave 1：核心数据与设置

- 文件：`crates/core/src/models.rs`、`settings.rs`、`store.rs`、`lib.rs`。
- 动作：建立 SQLite schema、迁移、设置模型、订阅与日程 CRUD；加入手动锁定、来源版本抑制、同步队列和客户端同步游标。
- 出口：`cargo test -p oshical-core` 通过 schema、幂等、手动锁定、删除抑制与同步队列测试。
- 安全：SQLite 使用参数绑定；敏感键拒绝写入数据库。
- 回滚：删除新表或恢复测试数据库，源码按文件恢复。
- 检查点：写 `handoff.md`。

### Wave 2：B 站与日程解析

- 文件：`bilibili.rs`、`schedule.rs`、`llm.rs`、测试文件。
- 动作：WBI 签名、buvid、搜索、卡片、动态、直播状态；文本/预约解析；视觉模型和缓存。
- 出口：真实搜索/直播接口烟雾测试；解析器测试全部通过；共享分析调用计数为 1。
- 安全：图片域白名单、文本长度限制、模型输出校验。
- 回滚：关闭图片分析配置仍保留文本和预约功能。
- 检查点：写 `handoff.md`。

### Wave 3：服务层、Axum 与 WebSocket

- 文件：`service.rs`、`server/src/main.rs`、`server/src/api.rs`、测试文件。
- 动作：业务协调、5 分钟动态刷新、20 秒开播轮询、事件日志、Bearer 鉴权、HTTP 路由、同步快照/队列接口和 WebSocket。
- 出口：健康检查、未授权 401、订阅 CRUD、同步快照/队列、WebSocket 鉴权与事件测试通过。
- 安全：绑定非回环无 Token 拒绝启动；CORS 白名单；日志脱敏。
- 回滚：服务端可单独回退到本地模式。
- 检查点：写 `handoff.md`。

### Wave 4：Tauri 桌面端

- 文件：`src-tauri/src/lib.rs`、`src-tauri/src/main.rs`、`src-tauri/tauri.conf.json`、capabilities、preload 契约。
- 动作：Tauri commands、Windows 凭据、通知点击、托盘与开机自启、远程 WebSocket 客户端、离线同步队列、本地后台任务。
- 出口：Tauri 开发构建成功；本地搜索/日程/通知命令可调用。
- 安全：命令参数校验、URL 主机白名单、密钥不出 Rust。
- 回滚：桌面壳失败时服务端 Web UI 仍可使用。
- 检查点：写 `handoff.md`。

### Wave 5：React 双端界面

- 文件：`src/main.tsx`、`src/App.tsx`、组件和样式。
- 动作：连续 7 天日程窗口、网格/时间线切换、1 天前后移动、搜索订阅、官号与别名、冲突处理、设置、连接状态、通知桥。
- 出口：浏览器和 Tauri 都能搜索、订阅、切换视图、前后移动 1 天、手动增改删日程、处理冲突；控制台无错误。
- 安全：无 `dangerouslySetInnerHTML`，Token 仅内存，错误文本不注入 HTML。
- 回滚：静态前端可独立回退。
- 检查点：写 `handoff.md`。

### Wave 6：验证、打包与文档

- 文件：测试、README、部署文档、启动脚本。
- 动作：全量测试、clippy、类型检查、真实接口烟雾、双模式 GUI 截图、托盘/通知/离线同步验证、Docker 构建和安装包构建。
- 出口：构建通过；测试计数和失败清单为 0；截图与接口响应留存。
- 安全：`cargo audit` 或 `cargo deny` 可用性检查；npm audit；敏感字符串扫描。
- 回滚：保留上一构建产物和数据库备份。
- 检查点：完成 session memory 与交付报告。

## 中断点

- 任一 Wave 编译失败：回到该 Wave 的最小代码切片修复，不进入下一 Wave。
- B 站接口风控：文本功能继续，记录未验证接口，不绕过风控。
- 通知点击无法实现：保留 Rust core 与 Axum，桌面壳决策重新评估。











## 第二轮澄清后的额外验收任务

- Wave 1 必须覆盖 `manual_locked`、`source_suppressions`、`sync_queue` 和 `client_sync_state`。
- Wave 3 必须覆盖服务器快照导出与离线 mutation 幂等提交，导入流程不得调用服务器写接口。
- Wave 4 必须验证关闭窗口进入托盘、托盘退出、开机自启开关和启动时已直播不通知。
- Wave 5 必须验证 7 天窗口每次移动 1 天、网格/时间线切换、右键和移动端长按菜单。
- Wave 6 必须验证 Docker 数据卷持久化、HTTPS 反向代理说明和数据库备份恢复。




## 实际执行结果（2026-10-26）

- Wave 1–5：已完成。
- `cargo fmt --all -- --check`：通过。
- `cargo test --workspace`：37 个测试通过，0 失败，1 个真实网络测试默认忽略；server API 4 项通过。
- 真实 B 站搜索与直播状态烟雾测试：通过。
- `cargo clippy --workspace --all-targets -- -D warnings`：通过。
- `npm run build:web`：通过，Vite 输出 JS 295.06 KiB。
- `npm audit --audit-level=high`：0 漏洞。
- 浏览器真实操作：登录、搜索、订阅、手动日程、自动预约抓取、时间线、网格和右键菜单通过。
- Tauri 真实操作：本地搜索、订阅、设置页、托盘驻留通过。
- `npm run build:portable` 与 `cargo build --release -p oshical-server`：通过；两个可执行文件已复制到 `release/`，并核对 SHA-256 与构建产物一致。
- 发布 Web UI：`release/server/dist/index.html` 已引用本轮构建的 JS/CSS 文件；保留的旧哈希 JS 文件未被入口引用。
- NSIS 安装程序 `release/windows/oshical-0.1.0-x64-setup.exe` 已生成；安装/卸载流程未在本机实测，MSI 未配置。
- 未完成：Docker 环境不存在；Windows 通知点击尚需人工确认；真实视觉模型准确率未验证。

## 2026-10-27 复核增量

- `cargo test --workspace`：核心 42 项、桌面 3 项、服务端运行时 1 项、API 4 项，共 50 项通过；2 项真实网络测试默认忽略。
- 数据库备份：新增 `SqliteStore::backup_to`，使用 SQLite `VACUUM INTO` 生成一致性快照并拒绝覆盖已有目标；回归验证备份可恢复订阅和日程。
- 显式运行 B 站真实接口烟雾测试：1 项通过；未配置 Cookie，未绕过风控。
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`npm run build:web`、`npm audit --audit-level=high` 均通过；npm audit 为 0 漏洞。
- 敏感字符串模式扫描命中 2 个预期文件：`bilibili.rs` 与 Tauri 桌面端的 Cookie 字段名/测试夹具；人工核对未发现真实 Token 或 Cookie，非完整 secret scanner。
- `cargo audit` 与 `cargo deny` 均未安装；本轮未擅自安装依赖，RustSec 级别依赖审计仍为空缺。
- 生成前端入口与 `release/server/dist/` 一致，JS/CSS 两份资源 SHA-256 相同。
- 7-Zip 完整性测试通过，安装包识别为 NSIS-3 Unicode/LZMA，7 个归档项；这仍不等于安装流程验收。
- Authenticode 检查显示桌面端、服务端与 NSIS 安装包均为 `NotSigned`。
- 新增一致性备份、B 站双登录、退出登录、扫码多来源 Cookie、来源跳转白名单和官号多人拆分后已重建桌面端/NSIS/服务端；当前发布副本与构建源一致：桌面 `3EA37A68...481335`、安装包 `FDDC0473...6B8417`、服务端 `DECDF342...8E9A7F`。
- 隔离 Linux Docker 验收通过：Ubuntu x86_64 / Docker 29.1.3 / Compose 2.40.3；镜像构建成功。健康检查与鉴权通过；真实 HTTP 创建订阅/日程并通过 WebSocket 收到事件；请求到事件约 354 ms；`docker compose down/up` 后容器 ID 变化但数据仍为订阅 1、日程 1，SQLite `integrity_check=ok`；Web UI、JS、CSS 均返回 200。测试数据随后已清理。
- 公网入口通过 Nginx 反向代理验证：HTTPS/WSS、Token 鉴权和证书校验通过；公网 WSS 事件延迟约 40.6 ms。正式部署需使用自有域名并配置证书自动续期。
- 本机独立服务端 EXE 启动仍被执行策略阻断，未作为验收证据；该项不再阻止已通过 Docker 的服务端运行验收。
- Wave 6 仍未完成：正式域名/证书续期、实际安装/卸载、Windows 通知点击、真实视觉模型准确性、RustSec 审计和代码签名待验收。

## 2026-10-27 B 站登录与搜索稳定性增量

- 搜索稳定性：`finger/spi` 和 WBI `nav` 初始化纳入最多 3 次网络重试；失败信息包含实际尝试次数；搜索缺少 `data.result` 时返回明确格式错误而不是静默空列表。
- 扫码登录：Rust 调用 B 站二维码生成/轮询接口，状态映射为 waiting/scanned/confirmed/expired；成功后仅提取 SESSDATA、bili_jct、DedeUserID、DedeUserID__ckMd5、sid 并保存到 Windows 凭据管理器。
- 浏览器登录：Tauri 创建独立 B 站登录 WebView，只允许 `bilibili.com`/`biligame.com` 域名；后台每秒读取 Cookie，检测到有效 SESSDATA 后自动保存并关闭窗口。
- 前端：设置页新增二维码弹窗和自动登录按钮；二维码使用本地 `qrcode` 包渲染，Cookie 不进入 React 状态、localStorage 或事件负载。
- 验证：完整 workspace 50 项通过、2 项真实网络测试忽略；真实二维码生成/轮询烟雾测试通过；隔离桌面实例实际显示二维码并加载 B 站登录页；本地主播库搜索实际返回结果。
- 未验证：由于没有用户 B 站账号凭据，未完成真实登录成功后的 Cookie 保存端到端操作。
- 二维码回调兼容：成功回调白名单增加 `biligame.com` 与子域名，修复 B 站扫码后 `BILI_QR_COOKIE` 错误；错误信息会显示不可信主机名。
- 主播库布局：修正搜索输入框 `width:100%` 与图标/按钮同排产生的 flex 溢出；结果文本列增加 `min-width:0` 和截断约束，横向滚动条已消失。
- 退出登录：新增 `clear_bilibili_cookie` 命令，删除 Windows 凭据管理器中的本机 Cookie、更新运行中 B 站客户端并广播状态；服务端环境变量不变。
- 扫码 Cookie：轮询成功时同时解析回调 URL、嵌套跳转 URL 和 `Set-Cookie`；只有缺失 SESSDATA 时返回参数名诊断，不输出 Cookie 值。
- 来源跳转：`open_external_url` 允许 `live.bilibili.com`、`t.bilibili.com`、`www.bilibili.com` 的 HTTPS 地址，其余域名仍拒绝。
- 官号多人：动态文字交给大模型一次返回多个 `performer` 事件，每个事件独立匹配主播；甲乙两条日程可分别入库，标题中的主播名会被清理。
- 动态文字：完全移除正则自由文本识别；模型未配置或失败时不猜测。文字分析按 post + 文本摘要 + 模型 + 文本 Prompt 版本缓存。


## 收尾复核：重新打开的门禁

2026-10-26 复核曾发现 Chat Completions 响应读取、慢动态阻塞开播探测及离线 mutation 持久化幂等缺口。上述缺口现已修复，并有 HTTP Mock、慢刷新虚拟时钟、迁移/重启/并发回执和 API 回归覆盖。剩余交付限制见 `docs/07-known-issues.md`；不以构建通过替代平台或真实服务验收。
