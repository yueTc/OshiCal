# 每天跑一次 + 长期稳定：部署与运维手册

> 场景：不常驻轮询，**每天跑一次**，跑完看一份变更日报。
> 目标：跑得稳、坏了知道、修得回来。

---

## 一、为什么日更比常驻更稳

| 维度 | 15 分钟轮询 | **每天一次** |
| --- | --- | --- |
| 每天请求数 | 1638 × 96 ≈ 15 万 | **3-10 个**（只抓变化的） |
| 触发限速概率 | 有 | 几乎为 0 |
| 进程存活时间 | 7×24，任何时刻可能挂 | 2 秒，挂的概率极低 |
| 内存/CPU 占用 | 常驻 | 跑完即释放 |
| 故障暴露 | 静默失败 | 退出码 + 看门狗双重告警 |

日更 + 增量后，稳态下**每天只发 3 个请求**（列表、全量列表、统计），
只有当天有角色数据变化时才额外拉详情。实测：

```
Day1 全量:  1641 请求, 59s  (角色 1638 / 历史 7325)
Day2 增量:    6 请求,  2s   (详情 3 个, 因为 3 个角色数据点变了)
Day3 增量:    3 请求,  2s   (当天无变化)
```

---

## 二、增量是怎么判断的

列表接口里每个角色已经带了 `dataPoints`、`nextPeriod`、`dayInCycle`。
这三元组就是免费的变化信号：

```
指纹 = [dataPoints, nextPeriod, dayInCycle]
指纹不变 → 不拉详情（省一个请求）
dataPoints 变 → 有新记录 → 拉详情
nextPeriod 变  → 预测窗口移动 → 拉详情
dayInCycle 变  → 只是天数在走，不拉（列表已够用）
```

指纹落盘在 `fingerprints.json`，跨进程、跨重启有效。

**注意**：这修正了一个致命缺陷。早期版本用"详情文件存在就跳过"做缓存，
结果新记录**永远抓不到**——因为文件早就在了。现在改成指纹驱动。

另外 `last_full_ts` 也持久化了。否则每次 `--once` 新进程 `last_full=0`，
会被当成首轮，天天全量重抓 1638 个详情（实测确实发生过）。

---

## 三、每天看什么

跑完自动生成两个文件：

### `日报-YYYY-MM-DD.md`

```markdown
# LAPLACE 变更日报 2026-11-07

- 统计区间：2026-11-07 17:15:56 之后
- 库内角色：1638 个，历史记录 7325 条

## 新增经期记录（2 条）

| 昵称 | uid | 经期开始 | 首次发现 | 当时预测误差 | 来源 |
| --- | --- | --- | --- | --- | --- |
| 铃音_Official | 39267739 | 2026-10-27 | 2026-11-07 17:16:05 | -1.0 | https://www.bilibili.com/opus/1263... |
| 铃音_Official | 39267739 | 2026-09-30 | 2026-11-07 17:16:05 | 1.0 | https://www.bilibili.com/opus/1253... |

## 生理状态变化（1 个）

| 昵称 | 变化 | 时间 |
| --- | --- | --- |
| 铃音_Official | 经期 → 易孕期 | 2026-11-07 17:16:05 |

## 新收录角色（0 个） / 下线角色（0 个）
```

### `changes-YYYY-MM-DD.jsonl`

同样的内容，机器可读，一行一条，`type` 区分 `new_history` / `status_change` / `added` / `removed`。

### 为什么需要 `first_seen_at`

站方给的是"经期开始日期"，不是"什么时候被提交的"。
只有自己记 `first_seen_at`，才能回答"**今天新增了什么**"。
这是日更场景的核心字段，也是长期积累下来最有价值的部分。

---

## 四、部署

### 4.1 目录

```
/opt/laplace/
├── laplace_daemon.py
├── laplace_scraper_http.py      # daemon 复用其中的 Client / flatten
├── requirements.txt
└── deploy/
    ├── laplace-crawl.service
    ├── laplace-crawl.timer
    ├── laplace-watchdog.service
    └── laplace-watchdog.timer

/var/lib/laplace/                # 数据（挂持久卷）
├── laplace.db                   # SQLite WAL
├── fingerprints.json
├── changes.jsonl                # 全量变更流水
├── changes-2026-11-07.jsonl     # 当日变更
├── 日报-2026-11-07.md
├── laplace_fertilities.csv/xlsx # 快照
├── metrics.prom
├── status.json
├── crawl.log
└── raw/details/{uid}.json
```

### 4.2 安装

```bash
sudo mkdir -p /opt/laplace /var/lib/laplace /var/log/laplace
sudo cp laplace_daemon.py laplace_scraper_http.py requirements.txt /opt/laplace/
sudo pip3 install -r /opt/laplace/requirements.txt

sudo cp deploy/laplace-crawl.service deploy/laplace-crawl.timer \
        deploy/laplace-watchdog.service deploy/laplace-watchdog.timer \
        /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now laplace-crawl.timer laplace-watchdog.timer

# 立即手动跑一次验证
sudo systemctl start laplace-crawl.service
journalctl -u laplace-crawl.service -n 40 --no-pager
```

### 4.3 cron 版（不想用 systemd）

```cron
# 每天 04:17 跑，失败不重试（有 cycle-retries 内部重试）
17 4 * * * cd /opt/laplace && /usr/bin/python3 laplace_daemon.py --out /var/lib/laplace --once --digest --cycle-retries 3 >> /var/log/laplace/cron.log 2>&1

# 每 3 小时检查一次，超过 30 小时没成功就告警
47 */3 * * * cd /opt/laplace && /usr/bin/python3 laplace_daemon.py --out /var/lib/laplace --check-age-hours 30 >> /var/log/laplace/watchdog.log 2>&1
```

### 4.4 Docker

```bash
docker run -d --name laplace-daily \
  -v /srv/laplace:/data \
  -e TZ=Asia/Shanghai \
  -e LAPLACE_ALERT_WEBHOOK=https://your-webhook \
  --restart unless-stopped \
  laplace-crawler \
  sh -c 'while true; do python laplace_daemon.py --out /data --once --digest --cycle-retries 3; sleep 82800; done'
```

镜像只需 `requests` + `openpyxl`，约 60MB。

---

## 五、稳定性是怎么保证的

### 5.1 实测记录

| 测试项 | 结果 |
| --- | --- |
| 连续 5 次 `--once` | 5/5 成功，每次 2s、3 请求、exit=0 |
| 单轮失败自动重试 | 3 次重试，指数退避 5s→10s→20s |
| 失败退出码 | **exit=1**（早期版本错误返回 0，已修） |
| 并发双实例 | 第二个 exit=3，锁互斥生效 |
| 崩溃后僵尸锁 | 自动识别并清理（Windows/Linux 分别判活） |
| 看门狗（30h 无成功） | 告警 + exit=1 |
| 磁盘不足 | 启动前检查，低于 200MB 拒绝运行并告警 |
| 数据库写入 | WAL 模式，原子写 tmp→rename |

### 5.2 七道防线

1. **HTTP 层**：连接池复用 + 指数退避重试（5 次）
2. **轮次级**：`--cycle-retries 3`，整轮失败重新来
3. **熔断层**：连续失败 5 次进入 30 分钟冷却，之后半开重试
4. **进程级**：PID 锁防重入，僵尸锁自动清理
5. **数据级**：SQLite WAL + 原子写，断电不会写坏
6. **调度级**：`Persistent=true`，关机错过的任务开机补跑
7. **监控级**：看门狗独立进程，主任务挂了也能报警

### 5.3 会坏的情况（诚实清单）

| 风险 | 概率 | 症状 | 应对 |
| --- | --- | --- | --- |
| 站方改版换接口 | 中 | `列表接口不可用` / 字段缺失 | 从 `_next` chunk 重新提取端点和字段 |
| 接口加鉴权/签名 | 低 | 全 401/403 | 改用 `laplace_scraper.py`（Playwright） |
| 机房 IP 被 Cloudflare 挑战 | 低 | 返回 "Just a moment..." | 挂住宅代理（`HTTPS_PROXY`） |
| 磁盘满 | 低 | 启动即拒绝 | 已有水位检查 + 告警 |
| 数据库被误删 | 极低 | 从头重建 | 全量重抓 1 分钟 |

**最可能的故障是站方改版**，其次是 IP 被封。两者都能从日报里立刻看出来：
角色数突然变 0、字段大面积为空、连续失败告警。

### 5.4 监控打点

`metrics.prom`（node_exporter textfile 格式）：

```
laplace_up 1
laplace_characters_total 1638
laplace_characters_current 998
laplace_history_total 7325
laplace_last_cycle_duration_seconds 2.13
laplace_last_cycle_requests 3
laplace_last_cycle_new_records 0
laplace_consecutive_failures 0
laplace_last_success_timestamp_seconds 1794014392
```

关键告警规则：

```yaml
- alert: LaplaceStale
  expr: time() - laplace_last_success_timestamp_seconds > 108000   # 30h
  for: 10m
- alert: LaplaceDown
  expr: laplace_up == 0
  for: 30m
- alert: LaplaceEmpty
  expr: laplace_characters_total < 1000      # 站方改版的强信号
  for: 1h
```

`status.json` 里同时有 `last_error`，排查时直接看。

---

## 六、日常操作

```bash
# 手动补跑
sudo systemctl start laplace-crawl.service

# 看今天变没变
cat /var/lib/laplace/日报-$(date +%F).md

# 看变更流水
tail -20 /var/lib/laplace/changes.jsonl

# 健康检查
python3 laplace_daemon.py --out /var/lib/laplace --check-age-hours 30

# 强制全量重抓（怀疑数据漂移时）
python3 laplace_daemon.py --out /var/lib/laplace --once --digest \
    --full-interval 0 --cycle-retries 3

# 用 SQL 查任意问题
sqlite3 /var/lib/laplace/laplace.db \
  "SELECT username, status, next_period FROM characters WHERE is_current=1 ORDER BY fans DESC LIMIT 10;"
```

---

## 七、一句话总结

**每天 3 个请求、2 秒跑完、产出 1 份日报、7 道防线兜底。**
长期稳定性不取决于爬虫写得多聪明，而取决于"**失败时你能不能立刻知道**"——
所以看门狗和退出码比增量逻辑更重要。
