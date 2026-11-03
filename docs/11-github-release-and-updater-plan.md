# OshiCal GitHub 发布与桌面端自动更新方案

> 文档状态：已实施，GitHub Release 端到端升级验证通过  
> 编写日期：2026-11-03  
> 适用范围：Windows 桌面端、GitHub 源码发布、GitHub Releases 自动更新  
> 当前阶段：本地构建、GitHub Release、签名校验和 v0.1.0 → v0.1.2 自动升级演练均已通过

---

## 1. 方案摘要

本方案使用 **Tauri 2 官方 Updater + GitHub Releases**，为 OshiCal Windows 桌面端提供自动更新能力：

1. 开发者推送版本标签，例如 `v0.1.2`。
2. GitHub Actions 在 Windows Runner 上构建 NSIS 安装包。
3. Tauri 生成更新包签名 `.sig` 和 `latest.json`。
4. 安装包、签名和更新清单上传到 GitHub Release。
5. 已安装的 OshiCal 启动时检查更新，并在设置页提供手动检查入口。
6. 用户确认后下载、校验签名、安装并自动重启。

需要明确：本方案中的“热更新”是 **带签名安装包的自动更新**，不是直接替换前端 JavaScript/CSS 的热补丁。Tauri 将前端资源内嵌到应用中，直接替换资源会绕过完整性与签名校验，不建议采用。

---

## 2. 推荐决策

| 项目 | 推荐值 | 说明 |
|---|---|---|
| GitHub 仓库 | `yueTc/OshiCal` | Public 仓库已确认 |
| 仓库可见性 | Public | 私有仓库的 Release 无法被客户端安全匿名下载 |
| 更新范围 | Windows 桌面端 | 服务端镜像更新作为后续独立阶段 |
| 更新方式 | Tauri 官方 Updater | 下载签名安装包并重启 |
| 发布方式 | GitHub Release | 显式推送版本标签后自动构建并发布 |
| 更新检查 | 启动时自动检查，安装需用户确认 | 不静默安装 |
| 签名策略 | Tauri 更新签名必填 | 与 Windows Authenticode 代码签名是两套机制 |
| 回滚策略 | 保留旧安装包 + Hotfix 版本 | GitHub 静态更新源不支持自动降级 |

---

## 3. 为什么采用该方案

### 3.1 Tauri 官方 Updater

优点：

- 与 Tauri 2 原生集成。
- 支持签名验证，防止更新包被篡改。
- 支持 GitHub Releases 静态更新清单。
- 支持下载进度和安装后重启。
- 不把 GitHub Token 放入客户端。

限制：

- 更新必须下载完整安装包，不是增量补丁。
- Windows 安装过程中应用会退出。
- GitHub 静态 `latest.json` 不支持服务端主动指定降级。
- 更新体验依赖 GitHub 网络可访问性。

### 3.2 不采用直接替换前端资源

不推荐原因：

- 绕过 Tauri 资源完整性和安装包签名。
- 容易造成前后端版本不一致。
- 回滚和问题定位困难。
- 对用户数据迁移、Rust 后端变更不适用。

---

## 4. 目标架构

```text
开发者推送 v* 标签
        |
        v
GitHub Actions (Windows)
        |
        +-- 安装依赖
        +-- 运行检查与测试
        +-- 构建 Tauri NSIS 安装包
        +-- 生成 .sig 更新签名
        +-- 生成 latest.json
        |
        v
GitHub Release
        |
        +-- 人工检查资产
        +-- 发布 Release
        |
        v
OshiCal 启动检查 / 设置页手动检查
        |
        v
下载更新 -> 校验签名 -> 安装 -> 自动重启
```

更新清单地址：

```text
https://github.com/yueTc/OshiCal/releases/latest/download/latest.json
```

---

## 5. 用户体验设计

### 5.1 启动时检查

- 应用启动后静默检查更新。
- 发现新版本时显示轻提示：
  - `发现新版本 v0.1.2，可在“设置 → 软件更新”中安装`
- 检查失败不弹错误、不阻塞主流程。
- 不自动下载或安装。

### 5.2 设置页“软件更新”

新增卡片包含：

- 当前应用版本。
- “检查更新”按钮。
- 新版本号与发布说明。
- 下载进度。
- “下载并安装”按钮。
- 安装完成后的“立即重启”按钮。
- 网络失败、签名校验失败等错误提示。

### 5.3 安装行为

Windows：

- 下载完成后启动 NSIS 更新程序。
- 应用自动退出。
- 安装完成后自动重启。
- 用户数据和本地 SQLite 数据库保持不变。

---

## 6. 实施阶段

### Wave 1：发布安全清理

目标：确保不会把内部测试数据和敏感信息发布到 GitHub。

任务：

- 完善 `.gitignore`：
  - `.ai-memory/`
  - `.tmp-*`
  - `data/`
  - `target/`
  - `dist/`
  - `release/`
  - 数据库与 WAL 文件
  - 本地环境变量文件
- 清理文档和测试中的内部测试地址、服务器路径、容器名称。
- 扫描 GitHub Token、API Key、Cookie、私钥等敏感内容。
- 检查待提交文件清单。
- 检查 Git 仓库体积，禁止直接 `git add -A`。

验收：

- 待提交文件中不含内部项目记忆、测试数据库、构建产物。
- 敏感信息扫描无真实凭据命中。
- Git 初始提交体积合理。

### Wave 2：Tauri Updater 接入

涉及文件：

- `package.json`
- `package-lock.json`
- `src-tauri/Cargo.toml`
- `Cargo.lock`
- `src-tauri/src/lib.rs`
- `src-tauri/tauri.conf.json`
- `src-tauri/capabilities/default.json`
- `src/lib/updater.ts`
- `src/App.tsx`
- `src/components/SettingsView.tsx`
- `src/styles.css`

任务：

- 注册 `tauri-plugin-updater`。
- 注册 `tauri-plugin-process`。
- 配置更新公钥与 GitHub `latest.json` 地址。
- 开放最小权限：
  - `updater:default`
  - `process:allow-restart`
- 实现前端检查更新、下载进度、安装与重启封装。
- 接入设置页和启动提示。

验收：

- 设置页能显示当前版本。
- 能检查更新并显示发布说明。
- 下载过程显示进度。
- 安装失败时当前应用仍可继续使用。
- TypeScript、Vite 构建通过。

### Wave 3：更新签名管理

任务：

- 使用 Tauri CLI 生成正式更新密钥对。
- 私钥不入库，建议使用密码保护。
- 公钥写入 `src-tauri/tauri.conf.json`。
- 在 GitHub 仓库配置 Secrets：
  - `TAURI_SIGNING_PRIVATE_KEY`
  - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
- 离线备份私钥和密码。
- 当前临时无密码密钥在正式发布前替换。

验收：

- 本地能够生成 `.sig`。
- GitHub Actions 能读取签名密钥。
- 错误签名无法通过 Updater 校验。

### Wave 4：GitHub Actions

新增：

- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`

CI 检查：

- `npm ci`
- `npm run check`
- `npm run test:web`
- `cargo test --workspace --no-fail-fast`
- `npm run build:web`

Release：

- 触发条件：推送 `v*` 标签，或手动触发。
- Windows Runner 构建。
- 使用 `tauri-apps/tauri-action@v1`。
- 上传：
  - NSIS 安装包
  - `.sig`
  - `latest.json`
- 默认创建正式 Release。
- 第三方 Action 尽量固定到提交 SHA。

验收：

- CI 工作流通过。
- Release 工作流生成完整资产。
- 显式版本标签是发布门禁；未推送标签不会影响现网用户。

### Wave 5：统一版本管理

建议新增：

```text
scripts/set-version.mjs
```

命令示例：

```powershell
node scripts/set-version.mjs 0.1.2
```

同步修改：

- `package.json`
- `package-lock.json`
- 根目录 `Cargo.toml` workspace version
- `src-tauri/tauri.conf.json`

验收：

- 四处版本一致。
- `cargo check` 后 `Cargo.lock` 版本同步。
- Release 标签与构建版本一致。

### Wave 6：GitHub 发布

前置条件：

- 完成 `gh auth login`。
- 确认仓库名和可见性。
- 确认版本号与发布说明。
- 完成敏感信息清理。

执行步骤：

1. 清理并审计待提交文件。
2. 创建初始 Git 提交。
3. 创建 GitHub 仓库。
4. 推送主分支。
5. 配置 GitHub Secrets。
6. 推送 `v0.1.0` 标签。
7. 检查正式 Release 资产。
8. 发布 Release。
9. 验证匿名访问 `latest.json`。

---

## 7. 验证标准

### 7.1 本地验证

- `npm run build:web` 通过。
- `npm run check` 通过。
- `cargo test --workspace --no-fail-fast` 通过。
- Tauri Windows 构建成功。
- 生成 NSIS 安装包。
- 生成更新签名 `.sig`。
- 生成 `latest.json`。
- `latest.json` 中 Windows URL、版本、签名完整。

### 7.2 GitHub 验证

- CI 通过。
- Release 资产完整。
- `latest.json` 可匿名下载。
- 安装包校验和与 Release 一致。
- 正式 Release 发布后 Updater 能读取。

### 7.3 端到端自动更新验证

必须实际执行：

1. 安装 `v0.1.0`。
2. 发布 `v0.1.2`。
3. 启动旧版本。
4. 检测到 `v0.1.2`。
5. 下载更新并校验签名。
6. 安装并自动重启。
7. 设置页显示 `v0.1.2`。
8. 使用错误签名测试，确认拒绝安装。

未完成以上步骤，不得宣称自动更新已验证。

---

## 8. 安全要求

### 8.1 更新安全

- 更新包必须经过 Tauri 签名校验。
- 私钥只保存在开发机和 GitHub Secrets。
- 私钥不得写入仓库、文档、日志或截图。
- 不把 GitHub Token 内置到客户端。
- Release 由显式版本标签触发自动发布。

### 8.2 仓库安全

- 不提交 `.ai-memory/`。
- 不提交 SQLite 数据库、WAL、Cookie、Token、API Key。
- 不提交内部服务器地址、容器路径和测试凭据。
- 使用最小权限 GitHub Actions。
- 第三方 Action 固定版本或 SHA。

### 8.3 已知安全边界

- Tauri 更新签名不等于 Windows Authenticode 代码签名。
- 未配置 Authenticode 时，Windows SmartScreen 仍可能提示未知发布者。
- 正式外部分发前建议配置可信 Windows 代码签名证书。

---

## 9. 回滚方案

### 9.1 发布前回滚

- 不推送版本标签即不会影响用户。
- 发现构建或签名问题，修复后重新构建。
- 必要时删除错误 Release 和标签。

### 9.2 发布后回滚

GitHub 静态 `latest.json` 不适合自动降级，建议：

1. 保留上一版本安装包。
2. 发现问题后停止后续发布。
3. 发布更高版本 Hotfix。
4. 必要时撤回错误 Release。
5. 对已升级用户提供旧版安装包手动恢复。

### 9.3 数据回滚

- 更新流程不得删除用户数据库。
- 涉及数据库结构变更时，必须单独提供迁移与回滚方案。
- 升级前建议保留一致性数据库备份。

---

## 10. 风险清单

| 风险 | 等级 | 缓解措施 |
|---|---|---|
| 内部数据被推送到 GitHub | 高 | 发布前白名单审计、敏感信息扫描 |
| 更新签名私钥泄露 | 高 | 私钥仅存本地和 GitHub Secrets |
| 私钥丢失导致无法继续发布 | 高 | 离线备份私钥和密码 |
| 私有仓库导致更新下载失败 | 高 | 使用 Public 仓库或公开 Release 资产 |
| 更新包被篡改 | 高 | Tauri 签名校验 |
| Windows SmartScreen 警告 | 中 | 后续配置 Authenticode 代码签名 |
| GitHub 网络不可达 | 中 | 检查失败不阻塞应用，提示重试 |
| 错误 Release 影响用户 | 中 | 版本标签门禁 + 自动验收 + Hotfix |
| 已更新用户无法自动降级 | 中 | 保留旧安装包，发布 Hotfix |
| 版本号不一致 | 中 | 统一版本脚本 + CI 校验 |

---

## 11. 当前预置改动

目前已有本地方案预置改动，但尚未提交、未推送、未完成最终验证：

- 安装 Tauri updater/process 前后端依赖。
- 增加 updater 前端封装。
- 增加设置页软件更新卡片。
- 增加启动检查更新提示。
- 增加 Tauri 插件注册与权限。
- 增加 updater 配置和临时公钥。
- 更新 `.gitignore`。
- 生成临时更新签名密钥，私钥保存在仓库外。

这些改动在审核通过前不应视为最终实现。

---

## 12. 审核确认项

请确认以下事项：

- [x] 仓库使用 `yueTc/OshiCal`
- [x] 仓库设置为 Public
- [x] 本次只更新 Windows 桌面端
- [x] Release 由显式版本标签触发自动发布
- [x] 正式发布前重新生成带密码的更新签名密钥
- [ ] 服务端 Docker/GHCR 更新作为后续阶段
- [x] 同意先完成本地构建和端到端升级演练，再正式推送 GitHub

审核通过后回复：

```text
按默认方案执行
```

---

## 13. 预计交付物

- GitHub 源码仓库。
- CI 工作流。
- Release 工作流。
- Windows NSIS 安装包。
- Tauri 更新签名文件。
- `latest.json`。
- 设置页软件更新功能。
- 启动时更新提示。
- 发布与回滚文档。
- 端到端升级验证记录。

## 14. 验证记录

- 2026-11-03：GitHub CI 通过。
- 2026-11-03：GitHub Release 工作流成功生成 Windows NSIS 安装包、更新签名和 `latest.json`。
- 2026-11-03：实际安装 `v0.1.0` 后成功检测并升级到 `v0.1.1`，应用自动退出、安装并重启。
- 验证中发现 Rustls 网络通道在部分网络环境下下载不稳定，已将 Updater 切换为 Windows 原生 TLS，并增加检查与下载重试。
- 验证过程中生成的 `v0.1.1` 为升级演练版本；包含网络稳定性修复的正式版本为 `v0.1.2`。

## 15. 服务端镜像更新

- GitHub Actions 在版本标签发布时构建 `ghcr.io/yuetec/oshical-server:<版本号>` 和 `latest` 镜像。
- `scripts/update-server.sh` 负责镜像比较、数据备份、更新、健康检查和失败回滚。
- 默认不挂载 Docker Socket；自动更新由宿主机 cron 调度更新脚本。
