# OshiCal Server 0.3.12

本次仅发布服务端，标签为 `server-v0.3.12`。桌面端保持 `0.3.6`，镜像内容与 0.3.10 / 0.3.11 相同。

## 为什么又需要一个版本号

`server-v0.3.11` 的 publish 阶段用的是**旧**的 `OSHICAL_SERVER_UPDATE_HMAC_KEY` 值，签出的
`latest-server.json` 是：

```
version  : 0.3.11
digest   : sha256:9c58cfbd06697e05d045c314ca02d41c11ec1839bd0d2664b498a329a1cc9f05
published: 489472a7407958ca9d08e73d326095e6c3635a4cb062ffe11931d29dac3ed5d5
```

宿主机密钥 `3a00bef3…` 对该清单计算的 HMAC-SHA256 是
`6cb7ac6e586948e703837dab90002301e9bec8fb4a148a1be80734022b7a66e9`，两者不同，说明
Secret 在 publish job 启动时尚未更新为宿主机使用的新密钥。

workflow 的 `Reject non-increasing server version` 不接受与已发布清单同级或更低的版本，
因此再发一个 0.3.12，让 publish 用已更新的 Secret 重签清单。

## 变更

- `server/Cargo.toml`：`0.3.11` → `0.3.12`。
- `Cargo.lock`：`oshical-server` 版本随之更新。
- 无代码、接口、数据库变更。

## 发布后核对

```bash
# 宿主机：清单里的 signature 必须等于用本机密钥重算的结果
python3 - <<'PY'
import hashlib, hmac, json
key = bytes.fromhex(open('/etc/oshical/updater-hmac.key').read().strip())
m = json.load(open('/opt/oshical-test/data/system/server-update-latest.json'))
payload = f"server\nghcr.io/yuetc/oshical-server\n{m['version']}\n{m['imageDigest']}".encode()
print(m['version'], m['signature'])
print('match:', hmac.compare_digest(hmac.new(key, payload, hashlib.sha256).hexdigest(), m['signature']))
PY
```

`match: True` 之后，`updater.log` 里不应再出现 `WARN unsigned update allowed`，可以移除
`/etc/systemd/system/oshical-updater.service.d/oshical-unsigned.conf` 并 `systemctl daemon-reload`
恢复强制验签。

## 验证

- 本机只做源码与格式检查，不执行构建、Clippy 或测试。
- Rust 格式、Clippy（`-D warnings`）、测试与服务器镜像构建由 GitHub Actions 完成。
