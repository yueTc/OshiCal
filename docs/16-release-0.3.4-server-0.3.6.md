# OshiCal 0.3.4 / Server 0.3.6

本次同时发布桌面端与服务端，标签为 `desktop-v0.3.4`、`server-v0.3.6`。

## 更新代理修复

- 修复宿主机 `oshical-updater.service` 因缺失 `LoadCredential` 文件而在脚本执行前直接失败的问题。
- 代理现在会始终写入心跳，并把 HMAC 密钥缺失、权限错误、格式错误和签名不匹配显示为可诊断的失败状态。
- 更新执行期间每 30 秒刷新一次心跳，避免长时间拉取、备份、重启或回滚被误判为代理离线。
- 新增 `scripts/install-oshical-updater.sh`：
  - 检查 HMAC 密钥的所有者、权限、硬链接和格式。
  - 安装代理脚本、systemd unit 和部署路径 drop-in。
  - 支持非 `/opt/oshical` 部署目录。
  - 提供 `--check` 只读自检，检查 timer、服务结果和心跳。
- 修正 GHCR 镜像仓库名拼写为 `ghcr.io/yuetc/oshical-server`。
- 设置页补充更新代理排障步骤和安装命令。

## 服务端 0.3.6

服务端 0.3.6 基于已合并的 0.3.4、0.3.5 修复继续发布，包含：

- 使用匿名 B 站弹幕 token。
- 动态缺少主题时复用同主播、同周期时段的历史日程或直播标题。
- 修复相关 LLM 结构化和日程合并测试。

## 验证

- 本机仅做源码、差异、Shell 语法和内嵌 Python 语法检查，不执行构建。
- 前端类型检查、Rust 格式、Clippy、测试、Windows 安装包和服务器镜像/便携包全部由 GitHub Actions 构建。
- 桌面端 Release 资产由 `.github/workflows/release.yml` 发布。
- 服务端镜像与签名清单由 `.github/workflows/server-image.yml` 发布。
- Linux/Windows 服务端压缩包由 `.github/workflows/server-binary.yml` 发布。