# OshiCal Desktop 0.3.8

本次仅发布桌面端，标签为 `desktop-v0.3.8`。服务端保持 `0.3.14`，不再重复发布。

## 换配色时牌标图案不再一会儿白一会儿黑

侧栏 logo、启动加载屏和登录卡片用的是同一个 `.brand-mark` 牌标，图案（日历轮廓 + 播放三角）吃 `currentColor`，而 `.brand-mark` 的颜色原先取自 `--on-primary`：

```css
.brand-mark { color: var(--on-primary); }
```

`--on-primary` 是 `appearance.ts` 里 `bestForeground()` 按对比度阈值算出来的**二值**结果，只会是 `#000000` 或 `#ffffff`：

| 配色 | 旧 `--on-primary` | 图案 |
| --- | --- | --- |
| 樱花粉 sakura | `#000000` | 黑 |
| 薄荷绿 mint | `#000000` | 黑 |
| 暖杏橙 amber | `#000000` | 黑 |
| 海盐蓝 ocean | `#ffffff` | 白 |
| 薰衣草 lavender | `#ffffff` | 白 |
| 蜜桃红 peach | `#ffffff` | 白 |
| 石墨灰 graphite | `#ffffff` | 白 |

于是每切一次配色，整块图案在黑白之间整体翻转；在自定义配色里拖动取色器、亮度越过阈值时，图案会来回闪。绿点的描边用的是 `var(--surface)`，自定义深色背景时还会跟着变暗，看起来像图案多了一圈黑边。

改法：牌标不再借用 `--on-primary`，改为**固定油墨 + 自适应底色**。

- `BRAND_MARK_INK = #fff8fb`，与 `public/oshical.svg`、`icon.ico` 里应用图标的油墨同色，界面 logo 和任务栏图标从此一致。
- 新增 `brandMarkPlate()`：把牌标底色按需压暗，直到固定油墨达到 3:1（WCAG 非文本图形阈值），最多迭代 20 次、每次 5% 黑，兜底 `#241820`。
- `applyAppearance()` 注入 `--brand-mark-ink` / `--brand-mark-from` / `--brand-mark-to` / `--brand-mark-plate`；`.brand-mark` 背景为 `linear-gradient(145deg, var(--brand-mark-from), var(--brand-mark-to))`，去掉了原来 `+24% 白` 的提亮——那一步会把白图案的对比度压到 2.4~3.0。
- 绿点描边由 `var(--surface)` 改为 `var(--brand-mark-plate)`，跟着牌标底色走。
- `:root` 补齐四个变量的默认值，JS 执行前的首帧也不会闪。

`--on-primary` 本身保留，继续服务 primary 按钮文字和配色选项上的对勾——那两处的黑白切换是保证可读性的正确行为。

七套预设实测（油墨恒为 `#FFF8FB`，对渐变两端取最差值）：

| 配色 | `--brand-mark-from` | 对比度 | `--brand-mark-to` | 对比度 |
| --- | --- | --- | --- | --- |
| 樱花粉 | `#d2477a` | 4.08 | `#ad2f5d` | 6.01 |
| 海盐蓝 | `#2a72be` | 4.72 | `#1b5795` | 7.06 |
| 薰衣草 | `#7d53dd` | 4.83 | `#623cbb` | 6.96 |
| 薄荷绿 | `#1c8f6d` | 3.86 | `#156c52` | 6.08 |
| 暖杏橙 | `#c36b14` | 3.69 | `#9c4e08` | 5.72 |
| 蜜桃红 | `#c3435a` | 4.70 | `#9c2a40` | 7.12 |
| 石墨灰 | `#4b5b6d` | 6.65 | `#334456` | 9.55 |

自定义配色的极端值同样达标：亮黄 `#ffe066` / 深底 → `#a18d40`（3.14）；极亮蓝白 `#d6f5ff` / 深底 → `#7d8f96`（3.22）；`#ff00aa` / 白底 → `#e60099`（4.16）；`#ff2fd0` / 深底 → `#e62abb`（3.71）。

## 随包携带 0.3.7 之后的核心修复

0.3.8 的桌面包编译的是当前 `main`，因此包含 0.3.7 之后合入的 `crates/core` 修复（服务端已随 `0.3.14` 先行发布）：

- 长日程图不再报 `Image size exceeds limit`：解码门槛 8192 → 24576、缩放目标 4096 → 16384、解码缓冲 128 MiB → 256 MiB，体积兜底增加短边下限 `IMAGE_MIN_SHORT_SIDE = 512`。细节见 `docs/26-release-server-0.3.14.md`。

## 兼容性

- 只改前端样式与外观变量注入，IPC 命令、HTTP 接口、数据模型都没有变化；本地库与 0.3.7 完全兼容。
- 牌标底色比 0.3.7 略深（深底 + 白图案），这是消除黑白翻转的必要代价；其余界面配色不变。
- `dist/` 未手工改动，随 CI 构建重新生成。

## 验证

- 本机仅做源码级验证（`AGENTS.md` 禁止在工作区产生构建产物）：用等价的对比度算法逐套配色与自定义极端色代入 `brandMarkPlate()`，确认油墨恒为 `#FFF8FB` 且对比度 3.14~9.55 全部 ≥3:1。
- 用 headless Edge 渲染七套配色的「修复前 / 修复后」对照图（`.tmp-brand-mark-preview.html` / `.tmp-brand-mark-preview.png`），修复前三套为黑图案、四套为白图案，修复后七套全部为白图案。
- 核对了 `src-tauri/icons/icon.ico` 的 16/24/32/48/64/256 六个尺寸帧：全部为粉底 + `#FFF8FB` 图案，尺寸之间无黑白差异，因此任务栏与托盘图标不存在同类问题。
- 按 `AGENTS.md`，格式检查、Clippy（`-D warnings`）、前端与 Rust 测试、NSIS 安装包、更新清单与签名全部由 GitHub Actions（`.github/workflows/release.yml`）完成。

## 发布资产

`OshiCal_0.3.8_x64-setup.exe`、`.sig`、`latest.json`（由 CI 生成）。
