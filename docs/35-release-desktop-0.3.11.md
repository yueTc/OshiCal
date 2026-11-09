# OshiCal Desktop 0.3.11

本次仅发布桌面端，标签为 `desktop-v0.3.11`。服务端同批发布 `0.3.19`（标签 `server-v0.3.19`），
两边共用同一份 `crates/core`，解析链路的改动完全相同。

## 变更

### 带图动态不再跳过文字分析

本地模式和应用内的嵌入服务端走的是同一套 `AppService::refresh_all` →
`process_source_posts_with_progress`（`crates/core/src/service.rs`）。旧逻辑在动态带图且开启
「自动分析图片」时，把正文整条短路掉，只把「图片 + 正文」一起送给图片模型；图片模型遇到
表情包、封面这类图返回空数组，正文里的时间点（「八点见」「今晚八点」「下周六播见」）随之
丢失。

0.3.11 起：

- 文字通道与图片通道是并集关系：带图动态照样把正文送一遍日程模型；
- 两条通道给出同一场时按时间槽 + 参与者去重（5 分钟容差），不会写两条日程；
- 图片模型的空结果不再当作「已分析」缓存，下次刷新会重试；
- 图片提示词改成「图文都可能是主信息来源」，正文写了时间就按发布时间所在自然日解析。

细节与代码位置见 [docs/34-release-server-0.3.19.md](34-release-server-0.3.19.md)。

### 历史上漏掉的场次

`prompt_version` 升到 `*-text-v12` / `*-image-context-v6-*`，本地库里带图动态的历史分析结果
全部失效。升级后触发一次刷新，之前被跳过的正文会重新走一遍模型，漏掉的场次补进日历。

## 兼容性

只改 `crates/core` 内的分析链路与提示词，IPC 命令、HTTP 接口、数据模型、数据库结构均无变化，
本地库与 0.3.10 完全兼容。图片分析的表结构与 `image_analysis_aliases` 逻辑不变，只是空结果
不再参与命中。

## 代价

带图动态会多一次文字模型调用。已经命中 `text-v12` 的动态不会重复计费；同一场只落一条。

## 构建与签名

按工作区约定不在本机构建，交由 CI：

- `cargo fmt --all -- --check`、`cargo clippy -D warnings`、`cargo test --workspace --locked`；
- `npm run build:desktop`（`tauri build`）产出 `OshiCal_0.3.11_x64-setup.exe` 与便携版；
- 更新签名沿用 `~/.tauri/oshical-updater.key`，`latest.json` 的 `windows-x86_64` 指向
  `desktop-v0.3.11` 的 NSIS 包。
