# OshiCal Server 0.3.17

本次仅发布服务端，标签为 `server-v0.3.17`。桌面端保持 `0.3.9`。

0.3.17 是 0.3.16 之后的**版本推进版**：日程解析与模型调度的行为与 0.3.16 完全一致
（`2.6` 版本号不再被当成日期、预约卡片交给日程模型这两处修复已在 0.3.16 落地），
本次不引入新的解析规则，风险面只有打包与流水线本身。同时把生理期缓存的采集方式
写进仓库，避免运维知识只存在于某一台机器上。

## 与前一个版本的关系

| 版本 | 内容 |
| --- | --- |
| `server-v0.3.15` | 预约卡片由正则决定日程，游戏版本号 `2.6` 被解析成 2 月 6 日并顺延到次年 |
| `server-v0.3.16` | 修日期解析；带卡片的动态一律交给日程模型，卡片正则降级为兜底 |
| `server-v0.3.17` | 行为同 0.3.16；补采集中服务的运维记录，重新产出镜像与二进制 |

如果你是 0.3.15 或更早，直接升到 0.3.17 即可，不必停在 0.3.16。

## 升级

Docker：

```bash
docker pull ghcr.io/yuetc/oshical-server:0.3.17
docker stop oshical && docker rm oshical
docker run -d --name oshical --restart unless-stopped \
  -p 127.0.0.1:8787:8787 \
  -v "$PWD/data:/app/data" \
  --env-file .env \
  ghcr.io/yuetc/oshical-server:0.3.17
```

二进制：

```bash
tar -xzf oshical-server-0.3.17-linux-x86_64.tar.gz -C /opt/oshical
cd /opt/oshical && ./oshical-server
```

数据兼容：`schedules` 表结构未变，无需迁移。回滚只需把镜像标签换回 `0.3.16`，
数据库可直接沿用。

## 生理期缓存采集（运维记录）

LAPLACE（`/ovu`）对容器内的 Rust 抓取会按 TLS 指纹回 `403` 挑战，因此采集放在宿主机上，
用 `curl_cffi` 的 Chrome 指纹完成，OshiCal 只读结果。

```
systemd: oshical-fertility-refresh.service
  /opt/oshical-test/venv/bin/python /opt/oshical-test/fertility-refresh/service.py
        └─ curl_cffi(chrome) → LAPLACE → upsert → data/oshical.db :: fertility_cache(id=1)
                                              ▲
                        OshiCal /api/fertility 只读缓存（毫秒级，带 stale 标记）
```

| 项 | 位置 |
| --- | --- |
| 服务单元 | `/etc/systemd/system/oshical-fertility-refresh.service`（enabled，Restart=always） |
| 配置 | `/opt/oshical-test/fertility-refresh/config.env` |
| 实现 | `refresh.py`（抓取+写库）、`service.py`（循环/退避/状态文件） |
| 状态 | `/var/lib/oshical-fertility/status.json` |
| 日志 | `journalctl -u oshical-fertility-refresh` |

节奏：正常 3 小时一轮（缓存 TTL 6 小时，留一半余量）；失败 300s 起退避，最长 1800s。
手工跑一轮不影响常驻进程的计时：

```bash
cd /opt/oshical-test/fertility-refresh
set -a; . ./config.env; set +a
/opt/oshical-test/venv/bin/python service.py --once
```

**不要用容器跑这套采集。** 曾经存在同名容器（`python:3.12-slim`，命令
`sh /app/entrypoint.sh`），但目录里并没有 `entrypoint.sh`，于是容器一直
`Restarting (2)`，几秒一次刷 `cannot open /app/entrypoint.sh`，还和 systemd 单元抢占
`/data` 与 `/deps` 挂载。该容器与空卷 `oshical-fertility-deps` 已于 2026-11-09 删除。
排查生理期问题只看 `systemctl status` 与 `journalctl`。

## 发布检查清单

- `cargo fmt --all -- --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 通过（CI 内执行）
- `cargo test --workspace --locked --no-fail-fast` 通过
- `Server Image`、`Server Binary` 两个工作流成功，Release 上挂有
  `oshical-server-0.3.17-linux-x86_64.tar.gz`、`-windows-x86_64.zip` 及对应 `.sha256`
