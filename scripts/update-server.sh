#!/usr/bin/env bash
set -Eeuo pipefail

cd "$(dirname "$0")/.."

if ! command -v docker >/dev/null 2>&1; then
  echo "未找到 docker 命令。" >&2
  exit 1
fi

if ! docker compose version >/dev/null 2>&1; then
  echo "需要 Docker Compose v2。" >&2
  exit 1
fi

export OSHICAL_IMAGE="${OSHICAL_IMAGE:-ghcr.io/yuetec/oshical-server:latest}"
health_url="${OSHICAL_HEALTH_URL:-http://127.0.0.1:${OSHICAL_PORT:-8787}/api/health}"
backup_dir="${OSHICAL_BACKUP_DIR:-backups}"
timestamp="$(date -u +%Y%m%d-%H%M%S)"
backup_file="$backup_dir/oshical-data-$timestamp.tar.gz"

mkdir -p "$backup_dir"
previous_image="$(docker compose images -q oshical | head -n 1 || true)"
rollback_image=""

if [[ -n "$previous_image" ]]; then
  rollback_image="oshical-server:rollback-$timestamp"
  docker tag "$previous_image" "$rollback_image"
fi

restore_previous() {
  if [[ -n "$rollback_image" ]]; then
    echo "正在恢复上一镜像: $rollback_image" >&2
    OSHICAL_IMAGE="$rollback_image" docker compose up -d --no-deps oshical || true
  else
    docker compose up -d --no-deps oshical || true
  fi
}

if ! docker compose pull oshical; then
  echo "拉取新镜像失败，保持当前服务不变。" >&2
  exit 1
fi

new_image="$(docker image inspect --format '{{.Id}}' "$OSHICAL_IMAGE" 2>/dev/null || true)"
if [[ -n "$previous_image" && "$previous_image" == "$new_image" ]]; then
  echo "当前已是最新镜像: $OSHICAL_IMAGE"
  exit 0
fi

echo "停止服务以生成一致性数据备份..."
docker compose stop oshical

if ! tar -C data -czf "$backup_file" .; then
  echo "数据备份失败，恢复原服务。" >&2
  restore_previous
  exit 1
fi
echo "数据备份完成: $backup_file"

if ! docker compose up -d --no-deps oshical; then
  echo "启动新镜像失败，恢复原服务。" >&2
  restore_previous
  exit 1
fi

for attempt in $(seq 1 30); do
  if curl -fsS "$health_url" >/dev/null 2>&1; then
    echo "服务更新成功: $OSHICAL_IMAGE"
    echo "健康检查: $health_url"
    exit 0
  fi
  sleep 2
done

echo "新版本健康检查失败，执行回滚。" >&2
restore_previous
exit 1
