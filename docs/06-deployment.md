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

`OSHICAL_PROXY_PORT` 可选，填写后服务器会把 B 站、模型和 LAPLACE 生理期等 HTTP 请求转发到服务器本机的 `127.0.0.1:<端口>`。也可以在 Web 管理界面的“网络代理”卡片中保存端口并点击“测试代理”，页面会显示经代理访问 Google 的延迟。

`OSHICAL_FERTILITY_URL` / `OSHICAL_FERTILITY_READER_URL` 可选，用来覆盖 LAPLACE 生理期数据的抓取地址。部分机房出口会被 Cloudflare 按 TLS 指纹做托管挑战（`403` + `cf-mitigated: challenge`），换 UA、补 Referer 都无效；此时把直连地址指向自建中转或镜像即可（容器内访问宿主机用 docker 网桥网关，通常是 `172.17.0.1`）：

```dotenv
OSHICAL_FERTILITY_URL=http://172.17.0.1:7861/laplace/fertilities?showOutdated=1
```

`/api/fertility` 不在请求路径上抓取：命中 6 小时缓存直接返回；缓存过期或为空时立即返回 `stale=true` 的缓存并在后台单飞刷新；`?refresh=1` 最多等待 15 秒。

服务器模式的 LAPLACE 采集只在设置页开启「月相」后运行，周期 6 小时；关闭开关即停止，不需要重启容器。

启动：

```powershell
docker compose up -d --build
```

也可以直接使用 GitHub Container Registry 镜像：

```powershell
docker compose pull
docker compose up -d --no-deps oshical
```

镜像地址：

```text
ghcr.io/yuetc/oshical-server:<版本号>
ghcr.io/yuetc/oshical-server:latest   # 正式版通道
ghcr.io/yuetc/oshical-server:beta     # 测试版通道
```

检查：

```powershell
Invoke-RestMethod http://127.0.0.1:8787/api/health
```

数据库持久化在 `./data`。


### 服务端镜像更新脚本

```bash
./scripts/update-server.sh
```

执行顺序：

1. 拉取 `OSHICAL_IMAGE` 对应镜像。
2. 比较当前容器镜像 ID，没有变化则直接退出，不重启服务。
3. 有变化时停止服务，生成 `backups/oshical-data-<时间>.tar.gz`。
4. 使用新镜像启动容器。
5. 最多等待 60 秒检查 `/api/health`。
6. 健康检查失败时自动切回上一镜像。

可配置变量：

```dotenv
OSHICAL_IMAGE=ghcr.io/yuetc/oshical-server:latest
OSHICAL_HEALTH_URL=http://127.0.0.1:8787/api/health
OSHICAL_BACKUP_DIR=backups
```

如果 GHCR 镜像为私有，需要先登录：

```bash
echo "$CR_PAT" | sudo docker login ghcr.io -u <GitHub用户名> --password-stdin
```

不建议让应用容器直接挂载 `/var/run/docker.sock`。需要自动更新时，可使用宿主机 cron 定时执行上述脚本。

### 客户端服务器更新

服务端模式设置页可显示：

- 当前服务器版本。
- 最新 GitHub Release 版本。
- 自动检测开关。
- 自动更新开关。
- 自动更新时间段。
- 手动立即更新。
- 更新代理在线状态、当前阶段和错误。

应用容器只写入受控更新请求，不挂载 Docker Socket。实际更新由宿主机 systemd 代理执行。

在服务器仓库目录安装代理（推荐）：

```bash
cd /opt/oshical

# 1) HMAC 密钥：内容必须与仓库 Secret OSHICAL_SERVER_UPDATE_HMAC_KEY 完全一致
#    （64 位小写十六进制字符），root 所有、0400 或 0600、单硬链接。
sudo install -d -m 0755 /etc/oshical
printf '%s\n' "<64位小写十六进制密钥>" | sudo tee /etc/oshical/updater-hmac.key >/dev/null
sudo chown root:root /etc/oshical/updater-hmac.key
sudo chmod 0400 /etc/oshical/updater-hmac.key

# 2) 安装代理脚本、systemd unit、部署路径 drop-in，并启动 timer
sudo bash scripts/install-oshical-updater.sh

# 3) 只读自检：密钥、timer、最近一次服务结果和心跳
sudo bash scripts/install-oshical-updater.sh --check
```

安装脚本会自动处理以下情况：

- 仓库就在 `/opt/oshical` 时，不会用 `install` 把文件覆盖到自身。
- 自动创建 `/etc/systemd/system/oshical-updater.service.d/oshical-deploy.conf`，设置实际部署目录和更新目录。
- 在启动代理后等待 90 秒内的新鲜心跳。
- 密钥缺失、权限错误或格式错误时直接给出可执行的修复命令。

如果部署目录不是 `/opt/oshical`：

```bash
sudo env OSHICAL_DEPLOY_DIR=/srv/oshical \
  bash /srv/oshical/scripts/install-oshical-updater.sh
```

如果需要自定义密钥或健康检查地址，可同时传入：

```bash
sudo env \
  OSHICAL_DEPLOY_DIR=/srv/oshical \
  OSHICAL_UPDATE_HMAC_KEY_FILE=/etc/oshical/updater-hmac.key \
  OSHICAL_HEALTH_URL=http://127.0.0.1:8787/api/health \
  bash /srv/oshical/scripts/install-oshical-updater.sh
```

手动安装时，如果仓库本身就在 `/opt/oshical`，不要执行 `install scripts/foo /opt/oshical/scripts/foo`，因为源文件和目标文件相同；此时只需确保权限：

```bash
sudo install -d -m 0755 /opt/oshical/scripts
sudo chmod 755 /opt/oshical/scripts/oshical-updater-agent.sh /opt/oshical/scripts/update-server.sh
sudo install -m 644 deploy/systemd/oshical-updater.service /etc/systemd/system/oshical-updater.service
sudo install -m 644 deploy/systemd/oshical-updater.timer /etc/systemd/system/oshical-updater.timer
sudo systemctl daemon-reload
sudo systemctl enable --now oshical-updater.timer
sudo systemctl start oshical-updater.service
```

GitHub Secret 无法读回。如果旧 HMAC 密钥已经丢失，不要把新生成的密钥当作现有发布版本的签名密钥；已发布清单仍会用旧密钥验签。此时应重新发布带新签名的服务器版本，或先按手动 Docker 流程更新当前版本。

代理每 15 秒检查一次请求。手动更新会立即执行；自动更新只在设置的时间段内执行。更新时间统一按 `Asia/Shanghai` 解释，开始时间晚于结束时间表示跨午夜。

更新代理会调用 `scripts/update-server.sh`，执行镜像比较、数据备份、容器重启、健康检查和失败回滚。

### 客户端显示「更新代理未在线」

服务端只负责写 `data/system/update-request.env`，真正干活的是宿主机代理。代理每次运行都会刷新 `data/system/updater-heartbeat.json`；服务端读到心跳超过 90 秒未更新（或文件不存在）就判定离线，客户端卡片显示「更新代理未在线」，同时禁用「立即更新服务器」。

按顺序排查：

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| `systemctl status oshical-updater.timer` 报 Unit not found | 未安装 | 运行 `cd /opt/oshical && sudo bash scripts/install-oshical-updater.sh` |
| timer 是 active，但 `updater-heartbeat.json` 不存在 | 代理脚本没跑起来、工作目录错误，或旧 unit 仍在等待缺失的 `LoadCredential` | 运行 `cd /opt/oshical && sudo bash scripts/install-oshical-updater.sh --check`，再查看 journal |
| 心跳存在、客户端仍显示未在线 | 容器看不到同一目录 | `docker compose exec oshical ls -l /app/data/system`，确认 `OSHICAL_UPDATE_DIR` 与卷挂载一致 |
| 代理在线，但点更新后状态变 `failed / validation` | HMAC 密钥缺失、权限不对或与发布签名不匹配 | 检查 `/etc/oshical/updater-hmac.key`（64 位小写十六进制、root、0400/0600），并与仓库 Secret 对比 |

日志：

```bash
sudo journalctl -u oshical-updater.service -n 200 --no-pager
cat data/system/updater.log
cat data/system/server-update-status.json
```

默认 GHCR 镜像可能为私有。服务器需先登录：

```bash
echo "$CR_PAT" | sudo docker login ghcr.io -u yueTc --password-stdin
```

设置页「服务器更新」卡片底部有「GitHub 镜像加速」子区块，例如填写 `https://gh-proxy.com/`。它随同卡片的「保存更新设置」一起提交，只影响服务器读取 GitHub 更新清单；不会影响桌面客户端的镜像设置。

客户端镜像加速在桌面端「软件更新」卡片内的同名子区块，点「保存」立即写入本机，仅作用于当前设备。

## 2. 便携服务端

从 GitHub Release `server-v<版本>` 直接下载编译好的压缩包，解压即可运行，不需要 Docker、Rust 或 Node.js：

| 平台 | 产物 | 内含 |
| --- | --- | --- |
| Linux x86_64 | `oshical-server-<版本>-linux-x86_64.tar.gz` | `oshical-server`、`dist/`、`.env.example`、`START-HERE.md`、`DEPLOYMENT.md` |
| Windows x86_64 | `oshical-server-<版本>-windows-x86_64.zip` | `oshical-server.exe`、`dist/`、`.env.example`、`START-HERE.md`、`DEPLOYMENT.md` |

每个包同时发布 `.sha256` 校验文件。编译由 `.github/workflows/server-binary.yml` 在标签 `server-v*` 上完成。

```powershell
# Windows
Expand-Archive oshical-server-0.3.6-windows-x86_64.zip -DestinationPath server
cd server
$env:OSHICAL_BIND = "0.0.0.0:8787"
$env:OSHICAL_API_TOKEN = "至少32位随机令牌"
$env:OSHICAL_WEB_DIR = "$PWD\dist"
$env:OSHICAL_DB_PATH = "$PWD\data\oshical.db"
.\oshical-server.exe
```

```bash
# Linux
tar -xzf oshical-server-0.3.6-linux-x86_64.tar.gz -C /opt/oshical
cd /opt/oshical
export OSHICAL_BIND=0.0.0.0:8787
export OSHICAL_API_TOKEN='至少32位随机令牌'
export OSHICAL_WEB_DIR="$PWD/dist"
export OSHICAL_DB_PATH="$PWD/data/oshical.db"
./oshical-server
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

## 7. 桌面端与服务端双渠道自动更新

桌面端和服务端均支持 `stable` 正式版与 `beta` 测试版渠道。测试版渠道同时比较两个清单并优先选择更高版本；用户从测试版主动切回正式版时，会立即检查最新正式版并询问是否安装。若正式版低于当前测试版，界面会明确提示降级风险，只有二次确认后才执行；自动检测和定时更新始终不会降级。

```text
桌面正式版：https://github.com/yueTc/OshiCal/releases/latest/download/latest.json
桌面测试版：https://github.com/yueTc/OshiCal/releases/download/desktop-beta/latest-beta.json
服务端正式版：https://github.com/yueTc/OshiCal/releases/download/server-latest/latest-server.json
服务端测试版：https://github.com/yueTc/OshiCal/releases/download/server-beta/latest-server.json
```

### 7.1 默认发布节奏

有改动时默认只前进一个 PATCH：

```powershell
npm run version:next-beta -- desktop
git tag desktop-v<桌面测试版>
git push origin desktop-v<桌面测试版>

npm run version:next-beta -- server
git tag server-v<服务端测试版>
git push origin server-v<服务端测试版>
```

- 首次测试版示例：`0.3.21-beta.1`。
- 同一 PATCH 的后续测试版只递增 `beta.N`，不再次前进 PATCH。
- 测试完成后必须询问是否发布正式版；确认后执行：

```powershell
npm run version:promote-stable -- desktop
npm run version:promote-stable -- server
```

- 正式版与测试版共享核心版本，例如 `0.3.21-beta.1` 提升为 `0.3.21`，不再额外增加 PATCH。
- 正式版发布前，不得开始下一 PATCH。MINOR / MAJOR 升级必须由用户当次明确要求。

### 7.2 发布检查

1. GitHub Actions 构建 NSIS 安装包、生成 `.sig` 与 `latest.json`。
2. 测试版 Release 必须标记 prerelease，并更新固定 `desktop-beta` 清单。
3. `server-v*` 测试版更新 `server-beta`，不得移动 Docker `latest`；正式版才更新 `server-latest`。
4. 安装旧版本，分别验证正式版与测试版渠道的检测、下载、签名校验、安装和重启。
5. 验证正式版与测试版之间的切换升级；确认不会静默安装低版本。

签名 Secrets：

```text
TAURI_SIGNING_PRIVATE_KEY
TAURI_SIGNING_PRIVATE_KEY_PASSWORD
```

Tauri 更新签名用于校验更新包完整性，不等同于 Windows Authenticode 代码签名。
