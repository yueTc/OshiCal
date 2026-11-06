# OshiCal 项目协作规范

## 构建约束（强制）

- 禁止在当前工作区或当前主机执行任何会产生构建/编译产物的命令，包括但不限于：
  - `cargo build`、`cargo test`、`cargo check`、`cargo clippy`、`cargo run`
  - `npm run build`、`npm run build:web`、`npm run build:desktop`、`npm run build:portable`
  - `npx tauri build`、`npx tauri dev`
  - `docker build`、镜像构建或其它本地编译流程
- 当前工作区只用于源码阅读、编辑、静态分析和文档修改。
- 需要构建、测试或产物验证时，必须使用 CI（GitHub Actions）或用户指定的外部构建机，不得在本工作区本地执行。
- 不得在 `.codex/worktrees`、用户目录或 C 盘创建 `target` 构建缓存；确需构建时只在用户明确指定的外部环境进行。
- 未经用户明确要求，不得删除或覆盖已有发布产物；新增产物应放在用户指定的发布目录。

## 例外

只有在用户当次明确要求“在本机构建”时，才可执行构建命令；执行前仍应确认输出目录，避免在 C 盘或 worktree 中产生 `target`。
