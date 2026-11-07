# OshiCal Server 0.3.11

本次仅发布服务端，标签为 `server-v0.3.11`。桌面端保持 `0.3.6`；没有服务端功能变更，版本号只用于重新签发服务器更新清单。

## 为什么需要一个新版本号

`server-latest` 是固定的发布通道，清单里的 `signature` 由仓库 Secret
`OSHICAL_SERVER_UPDATE_HMAC_KEY` 对 `server\n<镜像仓库>\n<版本>\n<镜像摘要>` 计算
HMAC-SHA256。Secret 无法读回，宿主机上的 `/etc/oshical/updater-hmac.key` 一旦丢失或
从未安装，就无法再用旧密钥复现签名。

发布 workflow 的 `Reject non-increasing server version` 会拒绝与已发布清单相同或更低的
版本，因此更换宿主机密钥后必须发一个更高的版本，用新密钥重签清单，宿主机再写入同一把
密钥，`signature_mismatch` 才会消失。

## 变更

- `server/Cargo.toml`：`0.3.10` → `0.3.11`。
- `Cargo.lock`：`oshical-server` 的版本随之更新。
- 镜像内容与 0.3.10 相同，无接口、数据库或行为变更。

## 宿主机侧配合操作

```bash
sudo install -d -m 0755 /etc/oshical
printf '%s\n' '<与仓库 Secret 完全一致的 64 位小写十六进制密钥>' \
  | sudo tee /etc/oshical/updater-hmac.key >/dev/null
sudo chown root:root /etc/oshical/updater-hmac.key
sudo chmod 0400 /etc/oshical/updater-hmac.key
sudo bash scripts/install-oshical-updater.sh --check
```

密钥必须与 Secret 逐字节一致（末尾单个换行可接受）。代理会校验普通文件、单硬链接、
`root` 所有、权限 `0400`/`0600` 和 64 位小写十六进制。

## 从 0.3.10 升级

- 客户端「服务器更新」卡片点「手动检查」应看到 `v0.3.11`，再点「立即更新服务器」。
- 代理按清单里的固定摘要拉取 `ghcr.io/yuetc/oshical-server@sha256:…`，先备份
  `data/`，再重启容器并做健康检查，失败自动回滚到旧容器。
- 升级后状态为 `succeeded / complete`，`updater.log` 里应没有
  `WARN unsigned update allowed`。

## 验证

- 本机只做源码与格式检查，不执行构建、Clippy 或测试。
- Rust 格式、Clippy（`-D warnings`）、测试与服务器镜像构建由 GitHub Actions 完成。
