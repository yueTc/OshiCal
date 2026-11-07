# laplace.live /ovu 爬虫

抓取 `https://laplace.live/ovu`（LAPLACE Cycles，虚拟主播生理周期追踪站）的全站数据。

## 两套方案

| 脚本 | 依赖 | 用途 | 实测 |
| --- | --- | --- | --- |
| **`laplace_scraper_http.py`** | 只要 `requests`+`openpyxl` | **服务器主方案** | 1638 详情 205s，零失败 |
| `laplace_scraper.py` | Playwright + Chromium | 被封时的兜底 | 1638 详情 43s |

**主方案是纯 HTTP。** 实测后端接口零校验：不带 Origin / Referer / Cookie，用默认 python-requests UA 也是 200。
详见 [DEPLOY.md](DEPLOY.md) 的完整实测表格与服务器部署方案。

```powershell
python laplace_scraper_http.py --out .\data --workers 6 --rate 6
```

## 反爬实测结论

| 层 | 现象 | 真相 |
| --- | --- | --- |
| `laplace.live` | 曾返回 429 + `X-Vercel-Mitigated: challenge` | 短时高频探测触发的 Vercel 限速，几分钟自愈；前端 HTML 不含数据，不用访问 |
| `workers.vrp.moe` | 曾返回 "Just a moment..." | 同样是限速/瞬时，不是永久挑战；正常速率下从不出现 |
| 并发 16+ | `ConnectionError` | 本地 socket/TLS 打爆，加连接池后 42 req/s 反而零错误 |
| 跨域 CSP | 页面内 fetch 后端报 `Failed to fetch` | 只影响浏览器，Python 端无关 |

## 接口（从 `_next` chunk 里挖出来的）


```
GET  https://workers.vrp.moe/laplace/fertilities            当前在册角色 (998)
GET  https://workers.vrp.moe/laplace/fertilities?showOutdated=1  含过期 (1638)
GET  https://workers.vrp.moe/laplace/fertility/{uid}        单角色详情 + history + userPreferences
GET  https://workers.vrp.moe/laplace/fertility-contribution-stats  投稿统计 + 预测准确率
GET  https://workers.vrp.moe/laplace/bilibili-user/{uid}    B站原始资料（含粉丝牌命）
GET  https://workers.vrp.moe/experiments/currency?symbols=CNY      汇率
POST https://workers.vrp.moe/laplace/fertility-contribute   投稿（只写，脚本不调用）
```

外部依赖（前端展示用，非本数据集）：`api.vtbs.moe`（VTuber 状态）、`vup-json.xn--7dvy22i.com`、`rsrc-cache.vrp.moe`、`eidolon.vrp.moe`。

## 运行

```powershell
# 全量：列表 + 统计 + 1638 个详情
python laplace_scraper.py --out .\laplace_out --concurrency 6

# 只要列表和统计
python laplace_scraper.py --out .\laplace_out --list-only

# 想看浏览器窗口（调试用）
python laplace_scraper.py --out .\laplace_out --headful
```

需要 `playwright` + Chromium：

```powershell
pip install playwright
python -m playwright install chromium
```

实测 1638 个详情 + 7325 条历史，并发 6 时 **43 秒**跑完。

## 离线重导出

`laplace_scraper.py` 会把原始响应存到 `<out>\raw\`，`export_offline.py` 只读这些文件重新生成
CSV / JSONL / SQLite / XLSX，不再联网、不需要浏览器：

```powershell
& "<带 openpyxl 的 python>" export_offline.py .\laplace_out
```

## 输出

| 文件 | 内容 |
| --- | --- |
| `laplace_fertilities.csv` | 1638 行 × 27 列，UTF-8 BOM，Excel 直接开 |
| `laplace_fertilities.xlsx` | 6 个 sheet：角色数据 / 状态分布 / 公会人数 / 投稿统计 / 按月准确率 / 历史记录 |
| `laplace_fertilities.jsonl` | 同上，一行一条 |
| `laplace_full.json` | 扁平字段 + 内嵌 `history` 数组 |
| `laplace_history.jsonl` | 7325 条经期记录明细（uid / 开始时间 / 提交时间 / B站动态来源 / 预测误差） |
| `laplace.db` | SQLite，`characters` / `history` / `meta` 三表，可直接 SQL |
| `README_报告.md` | 自动生成的统计报告 |
| `raw/` | 原始 JSON + 每个 uid 的详情 + 汇率，用于复现 |

## 字段

`status`：`normal` 常规 / `menstruating` 经期 / `fertile` 易孕期 / `ovulating` 排卵期
`liveStatus`：`0` 未开播 / `1` 直播中 / `2` 未开播
`predictionAccuracy.meanAbsErrorDays`：历史预测与实际经期开始的平均绝对误差，越小越准
`history[].source`：提交者引用的 B 站动态链接，可回溯原始证据
`history[].prediction`：当时模型给出的预测日期、误差天数、方法（`snapshot` 等）

## 注意

- 详情接口每个 uid 一次请求，1638 次；并发别开太大（默认 6 已够快），脚本内置 403/429 自动重建会话重试。
- `POST /laplace/fertility-contribute` 是投稿写接口，本脚本不调用。
- 数据里有用户昵称、B 站 uid、公会/MCN 归属，属于个人信息，落盘后自行控制流转。
