# Android 客户端（服务器模式）

> 文档状态：阶段性落地——源码改造完成，APK 由 CI 构建，尚未实测
> 编写日期：2026-11-07
> 适用范围：Android 客户端。iOS 共用同一套条件编译，但本轮不做 iOS 构建与验证
> 关联文件：`.github/workflows/android.yml`、`docs/07-known-issues.md`「安卓端（未实测）」

---

## 1. 目标与边界

Android 端的存在理由只有一条：**在手机上查看服务端已经算好的日程、直播状态和通知**。它不承担采集。

| 边界 | 决定 | 理由 |
| --- | --- | --- |
| 数据模式 | 只做服务器模式（服务端权威采集） | 手机后台进程随时可能被回收，抓取循环、SQLite 写入和 LLM 调用都不可靠；服务端已经是权威数据源 |
| 本地抓取 | 不上手机 | `spawn_local_monitor` 只在 `#[cfg(desktop)]` 内编译；移动端不启动任何抓取任务 |
| 通知 | 只在应用存活时发出 | 没有 FCM 通道，也没有前台服务；WebSocket 断开就不再产生通知 |
| 应用内更新 | 不做 | `tauri-plugin-updater` 没有 Android 实现，插件依赖本身也在移动端被移出编译；换版本靠侧载覆盖安装 |
| 分发 | 侧载（GitHub Actions artifact） | 不上架 Google Play，不做 AAB 审核流程，也不引入商店签名体系 |
| 凭据 | 应用私有目录下的 `secrets.json`（0600） | Android 没有 Windows 凭据管理器的等价物；应用私有目录由系统沙箱隔离 |

明确不做的：Google Play 上架、FCM 推送、后台长期保活、平板/横屏专用布局、iOS 真机验证。

---

## 2. 现状与差距

本轮只做静态源码改造，以下改动都已落盘；**没有任何一项经过构建或真机验证**。

| 项 | 改造前 | 改造后 | 状态 |
| --- | --- | --- | --- |
| 桌面专属插件 | `keyring`、`tauri-plugin-autostart`、`tauri-plugin-process`、`tauri-plugin-updater` 是全局依赖 | 移到 `[target.'cfg(not(any(target_os = "android", target_os = "ios")))'.dependencies]` | 源码已改；`tauri` 的 `tray-icon` feature 仍全局开启，业务代码必须用 `#[cfg(desktop)]` 隔离 `tauri::tray` |
| 入口与插件注册 | `run()` 无条件注册全部插件 | `#[cfg_attr(mobile, tauri::mobile_entry_point)]`；autostart/process/updater 注册在 `#[cfg(desktop)]` 块内；notification/opener 全平台 | 源码已改 |
| 桌面初始化 | `setup_window`、`setup_tray`、`spawn_local_monitor` 无条件执行 | 三者在 `#[cfg(desktop)]` 块内 | 源码已改 |
| 凭据存储 | 直接调用 `keyring` | 新增 `src-tauri/src/secrets.rs`：`SecretStore` trait，桌面用 keyring（服务名 `com.oshical.app`），Android/iOS 用 `data_dir/secrets.json`（临时文件 + rename 原子写，Unix 上收紧到 0600） | 源码已改；IPC 契约不变 |
| capabilities | `default.json` 覆盖所有平台 | `default.json` 加 `"platforms": ["linux", "macOS", "windows"]`；新增 `mobile.json`（`android`/`iOS`，只给 `core:default` + `notification:default` + `opener:default`） | 源码已改；移动端没有窗口控制、autostart、process 权限 |
| 窗口外壳 | 自定义无边框标题栏（`decorations: false`）在移动端同样渲染 | 前端用 `src/lib/platform.ts` 判定移动外壳：隐藏自定义标题栏与窗口控制按钮 | 源码已改：`platform.ts` 已导出 `isMobileShell`，`App.tsx`、`SettingsView.tsx`、`autostart.ts` 已接入 |
| 应用内更新入口 | 设置页始终显示「检查更新」 | 移动端不调用 `check_app_update`，设置页隐藏更新与自启卡片 | 源码已改 |
| 服务器连接入口 | 服务器地址与令牌表单只在 `tauri` 为真时渲染，移动端 `tauri` 为假会被一起隐藏 | `SettingsView` 新增 `mobileShell` prop：移动端仍渲染服务器地址与令牌表单，只把「本地/服务器」模式卡片换成一行说明 | 源码已改 |
| B 站登录 | 内嵌登录窗口 | 移动端 `open_bilibili_browser_login` 返回「移动端不支持内嵌 B 站登录窗口，请使用扫码登录或在服务端配置 Cookie」，走扫码 | 源码已改 |
| 数据模式 | 本地/服务器二选一 | 首次启动强制 `dataMode=server` | 源码已改：`App.tsx` 启动分支判定 |
| `.gitignore` | 整体忽略 `src-tauri/gen/` | 收窄为 `src-tauri/gen/schemas/`，让 CI 生成的 `gen/android` 工程壳可以提交 | 源码已改 |
| 前端单测 | 无平台判定测试 | 新增 `scripts/platform.test.ts`，随 `npm run test:web` 覆盖 `detectMobileShell` / `resolveDesktopShell` | 源码已改 |

尚未做的（不是遗漏，是本轮刻意留在范围外）：

- `src-tauri/gen/android` 工程壳还没生成（本工作区禁止构建）。`.gitignore` 的忽略范围已从 `src-tauri/gen/` 收窄为 `src-tauri/gen/schemas/`，因此 CI 首次 `npx tauri android init` 生成的工程壳可以直接提交回仓库；在提交之前，工作流每次都会重新生成。
- APK 从未构建过（本工作区禁止任何构建命令，见 `AGENTS.md`）。
- 没有任何真机安装、通知、扫码登录的实测证据。
- 应用签名只是「把材料放到约定位置」，Tauri 生成的 `app/build.gradle.kts` 默认不读 `key.properties`，真正签名还需要在该文件里补 `signingConfigs`。

---

## 3. 平台分层

一套 React + 一套 Rust 核心，平台差异集中压在三处：Cargo 条件依赖、Rust `#[cfg(...)]`、前端 `platform.ts`。

| 模块 | 桌面（Windows） | Android | 说明 |
| --- | --- | --- | --- |
| `crates/core` | 本地模式直接调用；服务器模式下作为服务端实现 | 只通过网络间接使用 | `core` 不依赖 Tauri/Axum，两端共用同一套解析、存储与业务规则，不做移动端分支 |
| `src-tauri`（Rust 壳） | 完整：托盘、自启、应用内更新、进程重启、本地抓取、内嵌 B 站登录 | 精简：只有通知与打开外链 | 平台差异由 `[target.'cfg(not(any(target_os = "android", target_os = "ios")))'.dependencies]` 与 `#[cfg(desktop)]` 表达；`run()` 用 `#[cfg_attr(mobile, tauri::mobile_entry_point)]` 暴露移动入口 |
| `src-tauri/src/secrets.rs` | `KeyringSecretStore`（Windows 凭据管理器） | `FileSecretStore` → `secrets.json` | 同一 `SecretStore` trait，空值表示删除，IPC 命令与返回结构不变 |
| `src-tauri/capabilities/` | `default.json`（linux/macOS/windows） | `mobile.json`（android/iOS） | 移动端不放行窗口控制、autostart、process restart |
| 前端 `src/lib/platform.ts` | `isMobileShell === false` | `isMobileShell === true` | 决定标题栏、窗口控制、更新入口、自启入口和首次启动的 `dataMode` |
| 前端其余代码 | 共用 | 共用 | 日程表、时间线、日历、主播库、设置页都是同一份实现 |
| 服务器 | 可选（本地模式不需要） | 必需 | Android 端没有服务端就没有数据 |

依赖方向不变：`React / Tauri / Server → core`。移动端不新增任何 `core` 反向依赖。

---

## 4. 构建流程

### 4.1 一次性初始化

```bash
npx tauri android init
```

生成 `src-tauri/gen/android`（Gradle 工程壳、`tauri.properties`、应用清单、图标映射）。这一步只需要做一次，生成的工程壳按项目惯例应该提交回仓库，让后续构建确定、也让签名配置有地方维护。

`.gitignore` 目前只忽略 `src-tauri/gen/schemas/`，`src-tauri/gen/android` 可以提交回仓库。首次运行由 `.github/workflows/android.yml` 的「初始化 Android 工程壳（首次）」步骤在目录缺失时执行 `npx tauri android init --ci`，产出后应把该目录提交，并删除这步兜底。

### 4.2 CI 构建

完整触发与步骤见 `.github/workflows/android.yml`，顺序为：

1. 检出源码（`actions/checkout`，pin SHA）
2. 安装 Node.js 24（`cache: npm`）
3. 安装 JDK 17（temurin）
4. 安装 Android SDK（`android-actions/setup-android@v3`）
5. 安装 NDK，并把 `ANDROID_HOME` / `ANDROID_SDK_ROOT` / `NDK_HOME` / `ANDROID_NDK_HOME` 写入 `$GITHUB_ENV`
6. 安装 Rust stable 与三个 Android 目标
7. Rust 缓存（`Swatinem/rust-cache`，`workspaces: ". -> target"`，`cache-on-failure`）
8. `npm ci`
9. `src-tauri/gen/android` 不存在则 `npx tauri android init --ci`
10. 展开签名材料（可选，缺 Secrets 时跳过）
11. `npx tauri android build --apk`
12. 上传 `src-tauri/gen/android/app/build/outputs/apk/**/*.apk`（artifact 名 `oshical-android-apk`）

触发方式：`workflow_dispatch` 手动运行，或推送 `android-v*` 标签。与 `desktop-v*`、`server-v*` 互不影响。

### 4.3 工具链版本

| 组件 | 版本 / 取值 | 来源 |
| --- | --- | --- |
| JDK | Temurin 17 | `actions/setup-java`（AGP 8.x 要求 17） |
| Android SDK | `setup-android@v3` 安装的 command-line tools，包由 `sdkmanager` 管理 | 工作流第 4、5 步 |
| NDK | `27.0.12077973` | 工作流 `env.NDK_VERSION`，改这里要同步改本文 |
| Rust | stable | `dtolnay/rust-toolchain`（pin SHA，与 `release.yml` 一致） |
| minSdk | 24 | `src-tauri/tauri.conf.json` 的 `bundle.android.minSdkVersion` |
| compileSdk / targetSdk | 由 Tauri 生成的工程壳默认值决定，本仓库未覆写 | `src-tauri/gen/android`（未入库） |
| Node.js | 24 | `actions/setup-node` |

### 4.4 ABI

| Rust target | Android ABI | 用途 |
| --- | --- | --- |
| `aarch64-linux-android` | `arm64-v8a` | 绝大多数真机 |
| `armv7-linux-androideabi` | `armeabi-v7a` | 32 位旧机型 |
| `x86_64-linux-android` | `x86_64` | 模拟器；x86_64 真机极少 |

三个目标都在工作流的 `dtolnay/rust-toolchain` 步骤里一次性装好。`tauri android build` 默认构建全部已安装目标；只想要真机包时可后续用 `--target aarch64` 收窄。

### 4.5 签名 Secrets

全部由 GitHub Actions Secrets 提供，仓库里不写任何密钥。

| Secret | 含义 | 缺失时 |
| --- | --- | --- |
| `ANDROID_KEY_BASE64` | 签名 keystore（JKS/PKCS12）的 base64 | 跳过签名步骤，产出未签名 APK，流水线不失败 |
| `ANDROID_KEY_ALIAS` | keystore 里的 key alias | 写入空值，签名必然失败，需与 keystore 一起配置 |
| `ANDROID_KEY_PASSWORD` | key 口令 | 同上 |
| `ANDROID_KEYSTORE_PASSWORD` | keystore 口令 | 同上 |

工作流把这些材料展开成 `src-tauri/gen/android/oshical-release.keystore` 与 `src-tauri/gen/android/key.properties`，内容不打印到日志。是否真正签名取决于工程壳的 `app/build.gradle.kts` 有没有读取 `key.properties`；Tauri 模板默认没有这段逻辑，所以首次签名前要先在工程壳里补 `signingConfigs`。

### 4.6 为什么不在这台机器上构建

`AGENTS.md` 明确禁止在当前工作区执行 `cargo build/test/check/clippy`、`npm run build`、`npx tauri build/dev`、`docker build` 等命令，本工作区只做源码、文档和静态分析；本机也没有 Android SDK/NDK 与 JDK 17，且不允许在 C 盘或 worktree 里生成 `target` 缓存。因此 APK 只能由 CI 产出，本地文档里不会出现「已构建」「已验证」这类结论。

---

## 5. 运行时行为

| 行为 | 实现要点 |
| --- | --- |
| 强制服务器模式 | 移动外壳首次启动把 `dataMode` 置为 `server`；没有「本机抓取」可用，本地模式在手机上不提供入口 |
| 设备注册 | 同步接口的 `platform` 取 `std::env::consts::OS`，Android 上即 `android`；服务端按 `device_id` 幂等注册，设备列表据此区分桌面与手机 |
| 连接 | 与 Web UI 相同的 HTTP + WebSocket：首帧 `{"type":"auth","token":...}`，服务端回 `auth_ok` 与当前 `seq` |
| 断线补拉 | 重连后用 `/api/events?after=<lastSeq>` 补齐遗漏事件，游标来自本地 `client_sync_state`；退避为 1/2/4/8/16/30 秒 |
| 通知 | 前台通知插件 `notification:default`；只在应用存活、WebSocket 收到 `live_status` 的 false → true 变化时触发，点击只允许打开 `https://live.bilibili.com/` |
| 无托盘 / 无自启 / 无进程重启 | 对应插件在移动端不编译也不注册，`mobile.json` 不给这些权限；前端隐藏相应设置卡片 |
| 无应用内更新 | `check_app_update` / `install_app_update` 在移动端返回「当前平台不支持应用内更新，请手动安装新版本」，设置页不渲染更新入口 |
| 服务器连接 | 移动端仍可填写服务器地址与访问令牌（设置页「运行模式」卡片换成一行说明），保存走同一条 `draft.dataMode === "server"` 分支 |
| 无内嵌 B 站登录 | 移动端改用扫码登录；Cookie 落盘走 `FileSecretStore`，不经过前端 |
| 无本地抓取循环 | `spawn_local_monitor` 只在桌面编译，手机不做动态轮询、不做 LLM 调用 |
| 数据迁移入口 | 移动端不传 `onImportServer` / `onMigrateToServer`，首次接入只保留「使用服务端数据」；迁移按钮置灰 |

---

## 6. 已知限制

- **后台被杀**：Android 会在内存紧张或厂商省电策略下回收进程，WebSocket 随之断开，日程与直播状态停止更新，通知也不再产生。恢复前台后靠游标补拉补齐，但期间的实时事件已经错过。
- **无 FCM**：没有系统级推送通道，应用不在前台就没有通知；也不做前台服务保活。
- **没做返回键与生命周期拦截**：不拦截系统返回键，默认退到桌面；进程被回收后重新进入时按快照/游标恢复，期间的实时事件不再补发。
- **自签证书不被信任**：Rust 侧使用 `rustls`，不读取系统信任库，服务端用自签证书或内部 CA 时 TLS 握手会直接失败（`docs/07-known-issues.md`「月相」小节记录的同一现象）。可用的办法是给服务端换公开 CA 证书，或在内置证书库中显式加入该 CA。
- **明文 HTTP 需要额外配置**：Android 9+ 默认禁止明文流量，服务端如果是 `http://` 地址，需要在工程壳的 `network_security_config.xml` 里显式放行对应域名/IP，否则请求被系统拦掉。当前工程壳未包含这份配置。
- **无应用内更新**：升级只有一条路——下载新 APK 覆盖安装。覆盖安装要求签名一致，换签名必须先卸载，卸载会清掉应用私有目录里的 `secrets.json`、本地缓存和服务端 Token。
- **未上架商店**：没有 Google Play 包（无 AAB、无商店签名、无审核流程），也没有应用商店的自动更新；分发与信任完全依赖侧载时用户手动放行。
- **签名链路未完成**：如 4.5 所述，工程壳默认不读 `key.properties`。
- **工程壳尚未入库**：`src-tauri/gen/android` 需要由 CI 首次 `tauri android init` 生成后提交回仓库（`.gitignore` 已放行该目录）。在提交之前，每次 CI 都会重新生成工程壳，工程壳里做过的手工改动（签名配置、`network_security_config`、权限声明）不会保留。

---

## 7. 验收清单

CI 产出 APK 之后逐项过；任何一项没过就不要对外说「安卓端可用」。

- [ ] 首次启动进入服务器模式：填服务器地址 + Token 能连上，`/api/health` 正常
- [ ] 登录后能拉到订阅列表与日程：从今天起连续 7 天数据与 Web UI 一致
- [ ] WebSocket 增量：服务端改一条日程，手机端在不手动刷新的情况下出现变化
- [ ] 断网补拉：飞行模式一段时间后恢复，事件按游标补齐且不重复、不丢条目
- [ ] 开播通知：服务端主播 false → true 时收到通知；点击后默认浏览器打开对应直播间
- [ ] 布局：竖屏/横屏旋转后不塌陷，刘海与手势条安全区正常，页面没有横向滚动
- [ ] 移动外壳：没有自定义标题栏与窗口控制按钮；设置页没有自启/更新卡片；没有托盘相关入口
- [ ] 扫码登录：二维码可显示、扫码成功后 Cookie 落到私有目录 `secrets.json`，重启应用仍有效
- [ ] 桌面零回归：同一次改动下 Windows NSIS 构建、托盘、自启、通知、应用内更新行为与改动前一致
- [ ] 权限：Android 13+ 通知权限申请路径确认（首次通知前是否已获授权）

---

## 8. 后续可选项

按性价比排序，都是独立小阶段，互不阻塞。

| 选项 | 收益 | 代价 |
| --- | --- | --- |
| 前台服务保活 | 显著降低后台被杀导致的断连与漏通知 | 需要常驻通知、`FOREGROUND_SERVICE` 声明和电量策略适配 |
| Keystore 加密 `secrets.json` | 明文口令文件降级为受系统密钥保护的密文 | 需要引入 Android Keystore 桥接（Rust 侧或 Kotlin 侧），并处理备份/换机解密失败 |
| FCM 推送 | 应用不在前台也能收开播通知 | 需要 Firebase 项目、服务端接入 FCM、国内网络可用性另说 |
| AAB 上架 | 商店分发、审核、自动更新 | 需要商店账号、AAB 构建、商店签名与合规材料 |
| 只构建 arm64 | CI 时间与 artifact 体积减半 | 模拟器与 32 位旧机装不上 |
| 工程壳入库 + 签名配置 | 构建可复现，签名链路闭环 | 要把 `gen/android` 提交进版本库，并在 `app/build.gradle.kts` 补 `signingConfigs` |

---

## 9. 与仓库约定的待确认项

1. **`src/lib/platform.ts`**：本文与 `.github/workflows/android.yml` 按其存在为前提描述移动外壳行为。该文件当前导出 `detectMobileShell(userAgent)`、`isMobileShell`、`resolveDesktopShell(tauri, mobile)`；强制服务器模式落在 `App.tsx` 的启动分支（`isMobileShell` 为真且 `settings.dataMode !== "server"` 时改写为 `server`）。若后续改名或改导出名，第 2、3、5 节要同步。
2. **`src-tauri/gen/android`**：`.gitignore` 已把忽略范围收窄为 `src-tauri/gen/schemas/`，生成的工程壳可以直接提交；本轮没有生成（禁止构建），首次 CI 运行后需要人工提交一次。
3. **`actions/upload-artifact`、`setup-java`、`setup-android`**：仓库现有工作流只 pin 了 checkout / setup-node / rust-toolchain / rust-cache；`server-binary.yml` 用的是 `upload-artifact@v4` 未 pin。新工作流按同样风格使用主版本号，并在行尾标了 `TODO: 无已核对 SHA`，补到 SHA 后替换。
4. **`tauri` 的 `tray-icon` feature 仍全局开启**：`tray-icon` crate 在 Tauri 的清单里只挂桌面 target，所以理论上 Android 构建会直接跳过它；若某些 Cargo 版本仍尝试为 Android 解析该可选依赖而报错，把它改成 `[target.'cfg(not(any(target_os = "android", target_os = "ios")))'.dependencies]` 下的 `tauri = { version = "2", features = ["tray-icon"] }` 声明即可，业务代码无需再改（`#[cfg(desktop)]` 已经隔离）。
