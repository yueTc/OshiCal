# 宿主更新代理 0.3.10 的 semver_regex 未定义

`scripts/oshical-updater-agent.sh` 在 0.3.10 加了「已安装版本状态」校验：

```bash
if [[ -n "$installed_version" && ! "$installed_version" =~ $semver_regex ]]; then
```

但脚本里从未定义 `semver_regex`。脚本本身跑在 `set -Eeuo pipefail` 下，所以第一次真正处理
更新请求时就会退出：

```
oshical-updater-agent.sh: line 717: semver_regex: unbound variable
```

症状与排查特征：

- 客户端显示 `更新代理在线`，点「立即更新服务器」后状态停在 `requested / queued`；
- `journalctl -u oshical-updater.service` 每 15 秒一条 `unbound variable`，service 结果 `exit-code`；
- `update-request.env` 一直留在 `data/system/` 里不被消费；
- 校验阶段之后的拉镜像、备份、重启都不会发生，所以容器版本不变。

## 修复

在校验之前定义该变量（与 `version_is_newer` 里的 Python 正则一致）：

```bash
semver_regex='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
```

已装到宿主机上的旧脚本需要单独替换（镜像不含宿主脚本）：

```bash
sudo install -m 0755 scripts/oshical-updater-agent.sh /opt/oshical/scripts/oshical-updater-agent.sh
sudo bash scripts/install-oshical-updater.sh --check
```

## 另一个已知取舍

宿主机未配置 HMAC 密钥时，代理会先失败在 `key_unreadable`。临时旁路只能通过
`OSHICAL_UPDATE_ALLOW_UNSIGNED=1` 显式打开（见 drop-in
`oshical-updater.service.d/oshical-unsigned.conf`），打开后状态文案仍会写「签名已验证」，
实际是放行未验签请求；恢复正式签名后应删除该 drop-in。
