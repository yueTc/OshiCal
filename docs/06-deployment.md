# 部署说明

## 1. Docker 部署

准备：

```powershell
Copy-Item .env.example .env
```

编辑 `.env`，至少设置：

```dotenv
OSHICAL_API_TOKEN=至少32位随机令牌
OSHICAL_LLM_API_KEY=日程模型密钥
OSHICAL_SECRETS_KEY=用于加密落库模型密钥的稳定主密钥
OSHICAL_LLM_MODEL=文字/视觉模型名
OSHICAL_LLM_BASE_URL=https://api.openai.com/v1
OSHICAL_PROXY_PORT=7890
```

`OSHICAL_PROXY_PORT` 可选，填写后服务器会把 B 站、模型和魔法期等 HTTP 请求转发到服务器本机的 `127.0.0.1:<端口>`。也可以在 Web 管理界面的“网络代理”卡片中保存端口并点击“测试代理”，页面会显示经代理访问 Google 的延迟。

启动：

```powershell
docker compose up -d --build
```

检查：

```powershell
Invoke-RestMethod http://127.0.0.1:8787/api/health
```

数据库持久化在 `./data`。

## 2. 便携服务端

```powershell
cd release/server
$env:OSHICAL_BIND = "0.0.0.0:8787"
$env:OSHICAL_API_TOKEN = "至少32位随机令牌"
$env:OSHICAL_WEB_DIR = "$PWD\dist"
$env:OSHICAL_DB_PATH = "$PWD\data\oshical.db"
.\oshical-server.exe
```

## 3. HTTPS 反向代理

公网部署必须使用 HTTPS。Nginx 示例：

```nginx
server {
    listen 443 ssl http2;
    server_name oshical.example.com;
    ssl_certificate /etc/letsencrypt/live/oshical.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/oshical.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8787;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
    }
}
```

WebSocket 必须保留 `Upgrade` 和 `Connection` 头。

## 4. 备份

推荐使用 SQLite 在线一致性备份；不要在服务运行且存在 WAL 时只复制主 `.db` 文件：

```powershell
New-Item -ItemType Directory -Force backups | Out-Null
$backup = "backups/oshical-$(Get-Date -Format 'yyyyMMdd-HHmmss').db"
sqlite3 data/oshical.db ".backup '$backup'"
sqlite3 $backup "PRAGMA integrity_check;"
```

`integrity_check` 必须返回 `ok`。项目核心同时提供 `SqliteStore::backup_to`，内部使用 `VACUUM INTO` 并拒绝覆盖已有目标；该能力已有自动化恢复回归。

如果无法使用 SQLite 备份工具，必须先正常停止服务并确认 `.db-wal`、`.db-shm` 已不存在，再复制主 `.db`。裸复制活动数据库主文件可能丢失 WAL 中的数据。

## 5. 回滚

1. 停止容器或服务端进程。
2. 恢复上一版本 `oshical-server.exe`。
3. 停止服务，恢复对应数据库备份；如果目标目录存在旧的 `.db-wal` 或 `.db-shm`，恢复到同一路径前必须先隔离这些陈旧侧车文件。
4. 启动并检查 `/api/health`。
5. 使用浏览器登录并检查订阅与日程。

## 6. 巡检

每日检查：

- `/api/health` 是否正常。
- 日志中 B 站风控、模型鉴权失败和 WebSocket 鉴权失败数量。
- 最近一次开播检测时间是否存在。
- 数据库文件大小和磁盘剩余空间。

每周检查：

- 执行数据库备份恢复演练。
- 检查订阅源 `lastError`。
- 检查模型费用和调用量。

## 7. Windows 桌面端自动更新

桌面端通过 Tauri 官方 Updater 读取 GitHub Releases 的 `latest.json`：

```text
https://github.com/yueTc/OshiCal/releases/latest/download/latest.json
```

发布流程：

1. 使用 `node scripts/set-version.mjs <版本号>` 同步 npm、Cargo 与 Tauri 版本。
2. 提交改动并推送 `v<版本号>` 标签。
3. GitHub Actions 构建 NSIS 安装包，生成 `.sig` 与 `latest.json`。
4. 检查 Draft Release 后发布。
5. 安装旧版本，确认客户端能检测、下载、校验、安装并重启到新版本。

签名 Secrets：

```text
TAURI_SIGNING_PRIVATE_KEY
TAURI_SIGNING_PRIVATE_KEY_PASSWORD
```

Tauri 更新签名用于校验更新包完整性，不等同于 Windows Authenticode 代码签名。
