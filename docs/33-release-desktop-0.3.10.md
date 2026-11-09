# OshiCal Desktop 0.3.10

本次仅发布桌面端，标签为 `desktop-v0.3.10`。服务端保持 `0.3.18`，不再重复发布。

0.3.9 之后，`main` 上一直没有「双击进入直播间」。该交互只存在于 `feat/android-client`
分支的 `845bc0c`（*feat(ui): 场次段数与直播质量展示调整*），`main` 从未包含。本版把这条
UI 线合回 `main`，并一并修掉配套的双击失效问题。安卓壳层相关提交仍不进入 `main`。

## 变更

### 双击进入直播间

| 位置 | 0.3.9 | 0.3.10 |
| --- | --- | --- |
| 日程卡片 | 单击整卡直接打开直播间，拖动时间线时容易误开浏览器 | 单击只做聚焦，**双击**（或键盘 `Enter`）才打开直播间 |
| 侧边栏主播条目 | 只有右键菜单 | **双击**（或 `Enter`）打开该主播直播间，右键（`Shift+F10`）打开操作菜单 |
| 触屏 | 双击被浏览器双击缩放吃掉 | `.schedule-card { touch-action: manipulation }`，`dblclick` 立即触发 |

### 修复：时间线下卡片双击静默失效

0.3.9 的 `src/components/ScheduleView.tsx` 在 `pointerdown` 当场调用 `setPointerCapture`，
后续 `dblclick` 被重定向到捕获元素（滚动容器），落在卡片上的双击永远收不到。0.3.10 改为：

- 指针捕获推迟到指针移动超过 6px 才设置——纯点击/双击不捕获，拖动才捕获；
- 时间线与网格两条渲染路径都挂 `onDoubleClick={handleBoardDoubleClick}`，配合
  `cardScheduleAtPoint(x, y)` 按坐标兜底命中，卡片本身不再各自处理双击；
- 卡片带 `data-schedule-id`，容器命中后按 id 取日程，避免事件重定向后拿不到目标。

### 主播库标题与搜索条吸附

滚动主播列表时标题与搜索条固定在窗口顶部（`creators-sticky`）。吸附条只在真正贴住窗口
顶部时（`is-pinned`）才铺底色与投影，未滚动时排版与原来一致，也不会盖住页面顶部的连接横幅；
桌面端吸附缝留 36px 避开右上角窗口按钮。搜索结果拆成独立卡片（`search-results-panel`），
不再占吸附区域高度。

同时把 `body` 与 `.app-shell` 的 `overflow-x` 从 `hidden` 改成 `clip`：`hidden` 会把祖先变成
「不滚动的滚动容器」，让 `position: sticky` 失效；`clip` 同样裁掉横向溢出，但不建立滚动容器。

### 设置页：镜像加速内嵌进更新卡片

`GithubMirrorCard` 重构为 `GithubMirrorField`，服务器与客户端两个 GitHub 镜像加速区块
分别内嵌在「服务器更新」和「软件更新」卡片内部，随同卡片的保存按钮一起提交，不再各自
一张卡、各有一个保存按钮。删除 `saveServerMirror` / `serverMirrorError`。

## 兼容性

- 只改前端交互与样式，IPC 命令、HTTP 接口、数据模型、数据库结构均无变化，本地库与 0.3.9 完全兼容。
- 升级后第一次打开若停在日程页，双击行为即生效，无需迁移或重建缓存。

## 已知限制

- 双击是唯一的主操作路径，单击不再跳转；习惯单击开播的用户需要重新适应，卡片聚焦后按 `Enter` 等价。
- 触屏端长按菜单与双击进直播间共存，长按后立即抬起可能被识别为一次点击，属平台手势判定，未额外拦截。

## 构建与签名

本次按用户当次要求在本机构建，未走 CI：

- `npm run build:desktop`（`tauri build`）→ `target/release/bundle/nsis/OshiCal_0.3.10_x64-setup.exe`；
- 前置 `tsc -b && vite build` 通过，Rust release 编译 1m04s；
- 更新签名使用 `~/.tauri/oshical-updater.key` + 对应密码，产出 `.sig` 与 `latest.json`（`windows-x86_64` 指向 `desktop-v0.3.10` 的 NSIS 包）；
- 未做 Authenticode 代码签名，`latest.json` 的 URL 需在资产上传到该标签后才会生效。

## 发布资产

- `OshiCal_0.3.10_x64-setup.exe`（NSIS 安装包，7.81 MiB）
- `OshiCal_0.3.10_x64-setup.exe.sig`（Tauri 更新签名）
- `latest.json`（自动更新清单）
- `OshiCal_0.3.10_x64-portable.exe`（便携版，`tauri build --no-bundle` 等价的 `oshical.exe` 改名副本）

