# OshiCal 0.3.5 / Server 0.3.7

本次同时发布桌面端与服务端，标签为 `desktop-v0.3.5`、`server-v0.3.7`。

## 更新状态显示修复

- 服务端更新进入 `running` 后，即使宿主机代理心跳因长时间拉取、备份、重启或回滚暂时超过 90 秒，也保留真实进度文案，不再改写为“等待宿主机更新代理上线”。
- 客户端在 `running` 状态优先显示“更新执行中”，继续展示目标版本、阶段和状态时间。
- 只有 `requested`、`waiting_window` 等确实等待代理处理的状态才显示代理离线排障面板。
- 客户端在 `requested`、`waiting_window`、`running` 期间禁用“立即更新服务器”，避免重复提交请求。
- 增加服务端单元测试，覆盖运行中心跳过期与排队等待代理两种情况。

## 兼容性

- 新客户端可以兼容仍由旧服务端返回“等待宿主机更新代理上线”的运行中响应：只要状态仍是 `running`，客户端会显示执行中并提示长任务期间心跳可能暂时中断。
- 宿主机更新代理仍需使用 0.3.6 起包含心跳保活和安装诊断修复的脚本。

## 验证

- 本机只做源码、差异和静态检查，不执行构建或测试。
- 前端类型检查、Rust 格式、Clippy、测试、Windows 安装包、服务器镜像和便携包全部由 GitHub Actions 构建验证。
- 桌面端 Release 资产由 `.github/workflows/release.yml` 发布。
- 服务端镜像与签名清单由 `.github/workflows/server-image.yml` 发布。
- Linux/Windows 服务端压缩包由 `.github/workflows/server-binary.yml` 发布。
