#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
unset CDPATH

usage() {
  cat <<'EOF'
用法:
  sudo bash scripts/install-oshical-updater.sh
  sudo bash scripts/install-oshical-updater.sh --check

可选环境变量:
  OSHICAL_DEPLOY_DIR           OshiCal Docker 部署目录，默认脚本所在仓库根目录
  OSHICAL_UPDATE_DIR           更新控制目录，默认 $OSHICAL_DEPLOY_DIR/data/system
  OSHICAL_UPDATE_HMAC_KEY_FILE HMAC 密钥路径，默认 /etc/oshical/updater-hmac.key
  OSHICAL_HEALTH_URL           本机健康检查 URL
  OSHICAL_SYSTEMD_DIR          systemd unit 目录，默认 /etc/systemd/system

安装模式会安装代理脚本、systemd unit 和部署路径 drop-in，然后启动 timer。
--check 只做只读检查，不安装或修改任何文件。
EOF
}

mode="install"
if (( $# > 1 )); then
  usage >&2
  exit 2
fi
case "${1:-}" in
  "") ;;
  --check) mode="check" ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "更新代理安装仅支持带 systemd 的 Linux 服务器。" >&2
  exit 1
fi
if [[ "$(id -u)" -ne 0 ]]; then
  echo "必须以 root 运行；请使用 sudo。" >&2
  exit 1
fi

for command in python3 install stat date sleep systemctl id uname mktemp chmod cat; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "缺少安装或自检所需命令: $command" >&2
    exit 1
  fi
done

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
project_root="$(cd -- "$script_dir/.." && pwd -P)"
deploy_dir="${OSHICAL_DEPLOY_DIR:-$project_root}"
if [[ ! -d "$deploy_dir" ]]; then
  echo "部署目录不存在: $deploy_dir" >&2
  exit 1
fi
deploy_dir="$(cd -- "$deploy_dir" && pwd -P)"
update_dir="${OSHICAL_UPDATE_DIR:-$deploy_dir/data/system}"
key_file="${OSHICAL_UPDATE_HMAC_KEY_FILE:-/etc/oshical/updater-hmac.key}"
health_url="${OSHICAL_HEALTH_URL:-http://127.0.0.1:${OSHICAL_PORT:-8787}/api/health}"
systemd_dir="${OSHICAL_SYSTEMD_DIR:-/etc/systemd/system}"

for value in "$deploy_dir" "$update_dir" "$key_file" "$health_url" "$systemd_dir"; do
  if [[ "$value" == *$'\n'* || "$value" == *$'\r'* || "$value" == *[[:space:]]* ]]; then
    echo "路径或 URL 不能包含空白、换行或回车: $value" >&2
    exit 1
  fi
done

for required in \
  "$deploy_dir/compose.yaml" \
  "$script_dir/update-server.sh" \
  "$script_dir/oshical-updater-agent.sh" \
  "$project_root/deploy/systemd/oshical-updater.service" \
  "$project_root/deploy/systemd/oshical-updater.timer"; do
  if [[ ! -f "$required" ]]; then
    echo "缺少代理安装所需文件: $required" >&2
    exit 1
  fi
done
if [[ ! -d "$deploy_dir/data" ]]; then
  echo "部署目录缺少 data/，请先在 $deploy_dir 完成 Docker 部署。" >&2
  exit 1
fi

check_key() {
  python3 - "$key_file" <<'PY'
import os
import re
import stat
import sys

path = sys.argv[1]
try:
    metadata = os.lstat(path)
except FileNotFoundError:
    raise SystemExit(f"HMAC 密钥不存在: {path}")
except OSError as error:
    raise SystemExit(f"无法读取 HMAC 密钥状态: {error.strerror or error}")
if not stat.S_ISREG(metadata.st_mode):
    raise SystemExit(f"HMAC 密钥必须是普通文件: {path}")
if metadata.st_uid != 0:
    raise SystemExit(f"HMAC 密钥必须属于 root: {path}")
if metadata.st_nlink != 1:
    raise SystemExit(f"HMAC 密钥必须是单硬链接文件: {path}")
if metadata.st_mode & 0o077:
    raise SystemExit(f"HMAC 密钥权限必须为 0400 或 0600: {path}")
try:
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
except OSError as error:
    raise SystemExit(f"无法打开 HMAC 密钥: {error.strerror or error}")
try:
    raw = bytearray()
    while True:
        chunk = os.read(fd, 4096)
        if not chunk:
            break
        raw.extend(chunk)
        if len(raw) > 4096:
            raise SystemExit("HMAC 密钥文件过大")
finally:
    os.close(fd)
try:
    value = bytes(raw).decode("ascii").strip()
except UnicodeDecodeError:
    raise SystemExit("HMAC 密钥必须是 ASCII 文本")
if not re.fullmatch(r"[0-9a-f]{64}", value):
    raise SystemExit("HMAC 密钥必须是恰好 64 位小写十六进制字符")
PY
}

install_file() {
  local source="$1" destination="$2" permissions="$3"
  if [[ -e "$destination" && "$source" -ef "$destination" ]]; then
    chmod "$permissions" "$destination"
  else
    install -m "$permissions" "$source" "$destination"
  fi
}

heartbeat_is_fresh() {
  local heartbeat="$1" modified now
  [[ -f "$heartbeat" ]] || return 1
  modified="$(stat -c '%Y' -- "$heartbeat" 2>/dev/null || true)"
  now="$(date +%s)"
  [[ "$modified" =~ ^[0-9]+$ ]] || return 1
  (( now >= modified && now - modified <= 90 ))
}

show_failure_diagnostics() {
  echo "--- oshical-updater.timer ---" >&2
  systemctl status oshical-updater.timer --no-pager -l >&2 || true
  echo "--- oshical-updater.service ---" >&2
  systemctl status oshical-updater.service --no-pager -l >&2 || true
  echo "--- 最近 80 行日志 ---" >&2
  journalctl -u oshical-updater.service -n 80 --no-pager >&2 || true
}

if [[ "$mode" == "check" ]]; then
  check_ok=true
  if check_key; then
    echo "[通过] HMAC 密钥权限与格式: $key_file"
  else
    check_ok=false
  fi

  if systemctl is-active --quiet oshical-updater.timer; then
    echo "[通过] oshical-updater.timer 正在运行"
  else
    echo "[失败] oshical-updater.timer 未运行" >&2
    check_ok=false
  fi

  heartbeat="$update_dir/updater-heartbeat.json"
  if heartbeat_is_fresh "$heartbeat"; then
    echo "[通过] 代理心跳在 90 秒内: $heartbeat"
  else
    echo "[失败] 代理心跳缺失或已过期: $heartbeat" >&2
    check_ok=false
  fi

  result="$(systemctl show -p Result --value oshical-updater.service 2>/dev/null || true)"
  if [[ "$result" == "success" ]]; then
    echo "[通过] 最近一次 oshical-updater.service 执行成功"
  else
    echo "[失败] 最近一次 oshical-updater.service 结果: ${result:-unknown}" >&2
    check_ok=false
  fi

  if [[ "$check_ok" == true ]]; then
    echo "自检通过。客户端应在 90 秒内显示「更新代理在线」。"
    exit 0
  fi
  show_failure_diagnostics
  exit 1
fi

if ! check_key; then
  cat >&2 <<EOF
更新代理未安装，因为 HMAC 密钥不可用。

密钥必须与 GitHub Actions Secret OSHICAL_SERVER_UPDATE_HMAC_KEY 完全一致：
  sudo install -d -m 0755 /etc/oshical
  printf '%s\\n' '<64位小写十六进制密钥>' | sudo tee $key_file >/dev/null
  sudo chown root:root $key_file
  sudo chmod 0400 $key_file

GitHub Secret 无法读回。如果旧密钥已经丢失，不要把新密钥当成现有发布版本的
签名密钥；需要重新发布服务器版本，或先按部署文档手动更新当前版本。
EOF
  exit 1
fi

install -d -m 0755 "$deploy_dir/scripts"
install_file "$script_dir/oshical-updater-agent.sh" "$deploy_dir/scripts/oshical-updater-agent.sh" 0755
install_file "$script_dir/update-server.sh" "$deploy_dir/scripts/update-server.sh" 0755

install -d -m 0755 "$systemd_dir"
install_file "$project_root/deploy/systemd/oshical-updater.service" "$systemd_dir/oshical-updater.service" 0644
install_file "$project_root/deploy/systemd/oshical-updater.timer" "$systemd_dir/oshical-updater.timer" 0644

dropin_dir="$systemd_dir/oshical-updater.service.d"
dropin_file="$dropin_dir/oshical-deploy.conf"
install -d -m 0755 "$dropin_dir"
dropin_tmp="$(mktemp "${TMPDIR:-/tmp}/oshical-updater-dropin.XXXXXX")"
trap 'rm -f -- "$dropin_tmp"' EXIT
readwrite_paths="/run/lock $deploy_dir"
if [[ "$update_dir" != "$deploy_dir" && "$update_dir" != "$deploy_dir/"* ]]; then
  readwrite_paths+=" $update_dir"
fi
cat >"$dropin_tmp" <<EOF
[Service]
WorkingDirectory=$deploy_dir
ExecStart=
ExecStart=/bin/bash $deploy_dir/scripts/oshical-updater-agent.sh
Environment=OSHICAL_UPDATE_DIR=$update_dir
Environment=OSHICAL_UPDATE_HMAC_KEY_FILE=$key_file
Environment=OSHICAL_HEALTH_URL=$health_url
ReadWritePaths=
ReadWritePaths=$readwrite_paths
EOF
install -m 0644 "$dropin_tmp" "$dropin_file"

systemctl daemon-reload
systemctl enable --now oshical-updater.timer
systemctl restart oshical-updater.timer

if ! systemctl start --no-block oshical-updater.service; then
  echo "代理已安装，但无法触发本次执行；下面保留具体诊断信息。" >&2
fi

heartbeat="$update_dir/updater-heartbeat.json"
heartbeat_ok=false
for _attempt in {1..20}; do
  if heartbeat_is_fresh "$heartbeat"; then
    heartbeat_ok=true
    break
  fi
  sleep 1
done
if [[ "$heartbeat_ok" != true ]]; then
  echo "代理未能写入新鲜心跳: $heartbeat" >&2
  show_failure_diagnostics
  exit 1
fi

echo "更新代理已安装并运行。"
echo "部署目录: $deploy_dir"
echo "心跳文件: $heartbeat"
echo "后续自检: sudo bash $deploy_dir/scripts/install-oshical-updater.sh --check"
echo "若存在排队更新，代理会在 timer 或本次触发中继续执行；失败原因会显示在设置页。"