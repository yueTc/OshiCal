# OshiCal 项目开发规范

## 本地编译位置

- **禁止在 C 盘执行编译、打包或产生构建缓存的命令。** 包括但不限于 `cargo check`、`cargo test`、`cargo clippy`、`cargo build`、`tsc`、`vite build`、`npm ci`、`npm run build:web`、`tauri build` 和 NSIS 打包。
- 本地源码可以先位于 C 盘工作树，但实际编译必须在 D 盘工作树或 D 盘临时检出目录中执行，确保 `dist/`、`target/`、npm 缓存和临时文件都不写入 C 盘。
- 推荐编译目录：`D:\work\OshiCal-build`。使用前确认目录对应当前提交，禁止覆盖或清理 `D:\work\guanrenchi`，该目录保留用户的未提交改动。
- D 盘不可用或空间不足时必须停止并说明原因，不得回退到 C 盘编译。

编译前在 D 盘工作目录设置：

```powershell
$buildRoot = 'D:\build\OshiCal'
$env:CARGO_TARGET_DIR = Join-Path $buildRoot 'cargo-target'
$env:CARGO_HOME = Join-Path $buildRoot 'cargo-home'
$env:TAURI_BUNDLER_TOOLS_CACHE_DIR = Join-Path $buildRoot 'tauri-bundler'
$env:npm_config_cache = Join-Path $buildRoot 'npm-cache'
$env:TEMP = Join-Path $buildRoot 'temp'
$env:TMP = $env:TEMP
New-Item -ItemType Directory -Force -Path $env:CARGO_TARGET_DIR, $env:CARGO_HOME, $env:TAURI_BUNDLER_TOOLS_CACHE_DIR, $env:npm_config_cache, $env:TEMP | Out-Null
```

编译前确认工作目录和关键路径均不在 C 盘：

```powershell
Get-Location
$env:CARGO_TARGET_DIR
$env:TEMP
```

GitHub Actions 的托管运行器不受本机目录规则限制；此规范针对本地开发、测试和打包。
