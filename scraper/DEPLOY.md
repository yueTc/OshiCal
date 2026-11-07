# 服务器部署：稳定性评估与落地方案

## 一、先给结论

| 问题 | 答案 |
| --- | --- |
| 能用纯 HTTP 跑吗？ | **能。不需要浏览器。** 接口没有任何校验：不带 Origin / Referer / Cookie，用默认的 `python-requests` UA 也是 200 |
| 之前为什么要 Playwright？ | 是我探测频率太高，把 Vercel 和 Cloudflare 的**限速**打出来了，误判成永久封锁 |
| 放服务器稳定吗？ | **稳。** 1642 个请求零失败；缓存续跑 3 秒完成 |
| 会被封 IP 吗？ | 只要不并发硬冲。实测 8 req/s 跑了 205 秒零失败；42 req/s 也能过，但没必要 |
| 需要代理池吗？ | 单机不需要。多实例/机房 IP 被挑战时再加 |

## 二、实测数据（都是本机跑出来的）

### 2.1 接口校验

```
裸 requests（默认 python-requests UA，无任何 header）→ 200
仅 UA，无 Origin/Referer                          → 200
带 Origin + Referer                               → 200
laplace.live 前端 15 次快速连打                   → 全部 200
workers 响应头                                     → Server: cloudflare, CF-RAY: hkg
```

后端是 Cloudflare 边缘，但没有开 Turnstile，也没有 Origin 白名单。`access-control-allow-origin` 返回 `None` 只影响**浏览器跨域**，Python 端不受影响。

### 2.2 速率阈值

| 模式 | 请求数 | 实际速率 | 结果 |
| --- | --- | --- | --- |
| 顺序无间隔 | 60 | 7.4 req/s | 60×200 |
| 顺序 0.3s | 40 | 2.2 req/s | 40×200 |
| 顺序 0.8s | 30 | 1.1 req/s | 30×200 |
| 并发 8，**每次新建连接** | 120 | 9.8 req/s | 120×200 |
| 并发 16，每次新建连接 | 200 | 12.2 req/s | 193×200 + **7 ConnectionError** |
| 并发 24，每次新建连接 | 240 | 11.7 req/s | 222×200 + **18 ConnectionError** |
| **Session + 连接池，并发 6** | 300 | **42 req/s** | **300×200** |
| **Session + 连接池，限速 8** | 1638 | 8.0 req/s | **1638×200** |

关键结论：

- 并发 16/24 的 `ConnectionError` 是**本地 socket / TLS 握手打爆**，不是站方封禁 —— 证据是换成连接池复用后，42 req/s 反而零错误。
- 全程**没有出现过 429 或 403**。
- 唯一出现过的 429 来自 `laplace.live` 前端，是我在几分钟内连续几十次探测触发的 Vercel 限速，几分钟自动恢复。而前端 HTML 本身不含数据，**服务器上根本不需要访问前端**。

### 2.3 缓存与增量

```
首次全量:  1638 详情, 205s, 请求 1642
二次运行:  1638 命中缓存, 3s, 请求 4（只有列表/统计/汇率）
```

详情按 `raw/details/{uid}.json` 落盘，重跑自动跳过。

## 三、部署

### 3.1 依赖

```bash
pip install requests openpyxl      # 两个包，没有别的
```

不需要 Chromium、不需要 Playwright、不需要系统字体。

### 3.2 目录

```
/app
├── laplace_scraper_http.py
├── requirements.txt
├── Dockerfile
└── /data                # 挂载卷
    ├── raw/             # 原始响应 + details/{uid}.json
    ├── laplace.db
    ├── laplace_fertilities.xlsx
    └── ...
```

### 3.3 运行

```bash
# 增量（每日跑）
python laplace_scraper_http.py --out /data --workers 6 --rate 6

# 只刷新列表和统计，不碰详情（每 10 分钟跑，成本 3 个请求）
python laplace_scraper_http.py --out /data --no-details

# 强制全量重抓（数据模型变更时）
python laplace_scraper_http.py --out /data --refresh
```

### 3.4 Docker

```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY laplace_scraper_http.py .
ENV TZ=Asia/Shanghai
CMD ["sh", "-c", "python laplace_scraper_http.py --out /data --workers 6 --rate 6"]
```

```bash
docker build -t laplace-crawler .
docker run -d --name laplace -v /srv/laplace:/data --restart unless-stopped laplace-crawler
```

镜像约 60MB（对比 Playwright 方案的 ~1.2GB）。

### 3.5 定时

**cron（每天 04:00 增量）**
```cron
0 4 * * * cd /app && python laplace_scraper_http.py --out /data --workers 6 --rate 6 >> /var/log/laplace.log 2>&1
```

**systemd timer**
```ini
# /etc/systemd/system/laplace-crawl.service
[Unit]
Description=LAPLACE crawler
After=network-online.target

[Service]
Type=oneshot
WorkingDirectory=/app
ExecStart=/usr/bin/python3 laplace_scraper_http.py --out /data --workers 6 --rate 6
```

```ini
# /etc/systemd/system/laplace-crawl.timer
[Unit]
Description=Daily LAPLACE crawl

[Timer]
OnCalendar=*-*-* 04:00:00
Persistent=true

[Install]
WantedBy=timers.target
```

**K8s CronJob**
```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: laplace-crawler
spec:
  schedule: "0 4 * * *"
  jobTemplate:
    spec:
      template:
        spec:
          restartPolicy: OnFailure
          containers:
            - name: crawler
              image: laplace-crawler
              args: ["python","laplace_scraper_http.py","--out","/data","--workers","6","--rate","6"]
              volumeMounts:
                - {name: data, mountPath: /data}
          volumes:
            - name: data
              persistentVolumeClaim: {claimName: laplace-data}
```

## 四、故障处理

| 症状 | 原因 | 处理 |
| --- | --- | --- |
| 全部 429 | 本机出网 IP 短时间内被打太多 | 脚本内已有指数退避；降 `--rate` 到 2，等 5 分钟 |
| 出现 `Just a moment...` | 该 IP 段被 Cloudflare 挑战 | 换出口 IP / 挂住宅代理；此时才需要 Playwright 兜底 |
| 大量 ConnectionError | 并发太高，本地 socket 耗尽 | 降 `--workers`，确认单 Session 连接池复用 |
| 某个 uid 一直 404 | 角色已从站方删除 | 正常，脚本返回 None 并跳过 |
| 详情比列表多 | `showOutdated=1` 含已过期角色 | 正常，`在册` 字段区分 |

## 五、监控建议

抓完后从 `laplace.db` 取三个数打点，任何一项突变都说明结构变了：

```sql
-- 1. 角色总数（正常 1630-1650 缓慢增长）
SELECT COUNT(*) FROM characters;
-- 2. 历史记录总数（正常 7300+ 单调增长）
SELECT COUNT(*) FROM history;
-- 3. 最新一条记录的日期（正常在 7 天内）
SELECT MAX(period_start_date) FROM history;
```

再加两个健康信号：

- 本次运行请求数 / 上次运行请求数 → 突然暴涨说明缓存失效
- 失败请求数（stderr 里的 `[失败]`）→ 应为 0

## 六、如果哪天真的被封了

Playwright 那套代码还留着（`laplace_scraper.py`），它是**兜底**而不是主方案：

```bash
python laplace_scraper.py --out ./fallback --concurrency 6
```

原理：真 Chromium 过 Vercel checkpoint → 在 `workers.vrp.moe` 源下开标签页做同源 fetch（绕开 CSP `connect-src`）→ 同源请求天然带 `cf_clearance`。服务器上跑需要装 Chromium：

```bash
pip install playwright && python -m playwright install --with-deps chromium
```

代价：镜像 +1.1GB、内存 +400MB、每次冷启动多 8-15 秒过挑战。**只在纯 HTTP 方案被挑战时才启用。**
