#!/usr/bin/env bash
set -Eeuo pipefail

cd "$(dirname "$0")/.."

update_dir="${OSHICAL_UPDATE_DIR:-$PWD/data/system}"
request_file="$update_dir/update-request.env"
status_file="$update_dir/server-update-status.json"
heartbeat_file="$update_dir/updater-heartbeat.json"
mkdir -p "$update_dir"

timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
printf '{"updatedAt":"%s"}\n' "$timestamp" > "$heartbeat_file"

escape_json() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr '\n' ' '
}

write_status() {
  local status="$1" phase="$2" message="$3" error="${4:-}" target="${5:-}"
  local error_json="null" target_json="null"
  if [[ -n "$error" ]]; then
    error_json="\"$(escape_json "$error")\" "
  fi
  if [[ -n "$target" ]]; then
    target_json="\"$(escape_json "$target")\" "
  fi
  printf '{"status":"%s","phase":"%s","message":"%s","targetVersion":%s,"updatedAt":"%s","error":%s}\n' \
    "$status" "$phase" "$(escape_json "$message")" "$target_json" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$error_json" \
    > "$status_file"
}

if [[ ! -f "$request_file" ]]; then
  exit 0
fi

request_id=""
target_version=""
force="false"
window_start="03:00"
window_end="05:00"
while IFS='=' read -r key value; do
  case "$key" in
    request_id) request_id="$value" ;;
    target_version) target_version="$value" ;;
    force) force="$value" ;;
    window_start) window_start="$value" ;;
    window_end) window_end="$value" ;;
  esac
done < "$request_file"

if [[ ! "$request_id" =~ ^[0-9a-fA-F-]{36}$ ]]; then
  write_status "failed" "validation" "更新请求 ID 无效" "invalid request id"
  rm -f "$request_file"
  exit 1
fi

if [[ ! "$target_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  write_status "failed" "validation" "目标版本号无效" "invalid target version"
  rm -f "$request_file"
  exit 1
fi

if [[ ! "$window_start" =~ ^([01][0-9]|2[0-3]):[0-5][0-9]$ || ! "$window_end" =~ ^([01][0-9]|2[0-3]):[0-5][0-9]$ ]]; then
  window_start="03:00"
  window_end="05:00"
fi

now_hhmm="$(TZ=Asia/Shanghai date +%H:%M)"
in_window() {
  if [[ "$window_start" == "$window_end" ]]; then
    return 0
  fi
  if [[ "$window_start" < "$window_end" ]]; then
    [[ "$now_hhmm" > "$window_start" || "$now_hhmm" == "$window_start" ]] && [[ "$now_hhmm" < "$window_end" ]]
  else
    [[ "$now_hhmm" > "$window_start" || "$now_hhmm" == "$window_start" || "$now_hhmm" < "$window_end" ]]
  fi
}

if [[ "$force" != "true" ]] && ! in_window; then
  write_status "waiting_window" "waiting_window" "等待自动更新时间段" "" "$target_version"
  exit 0
fi

health_url="${OSHICAL_HEALTH_URL:-http://127.0.0.1:${OSHICAL_PORT:-8787}/api/health}"
current_version="$(curl -fsS "$health_url" 2>/dev/null | sed -n 's/.*"version":"\([^"]*\)".*/\1/p' | head -n 1)"
if [[ "$current_version" == "$target_version" ]]; then
  write_status "succeeded" "complete" "服务器已经是最新版本" "" "$target_version"
  rm -f "$request_file"
  exit 0
fi

write_status "running" "pulling" "正在拉取服务器镜像" "" "$target_version"
if OSHICAL_IMAGE="ghcr.io/yuetc/oshical-server:$target_version" ./scripts/update-server.sh >> "$update_dir/updater.log" 2>&1; then
  refreshed_version="$(curl -fsS "$health_url" 2>/dev/null | sed -n 's/.*"version":"\([^"]*\)".*/\1/p' | head -n 1)"
  write_status "succeeded" "complete" "服务器更新成功" "" "${refreshed_version:-$target_version}"
  rm -f "$request_file"
  exit 0
fi

write_status "failed" "failed" "服务器更新失败，已尝试回滚" "请检查 updater.log 和容器日志" "$target_version"
rm -f "$request_file"
exit 1
