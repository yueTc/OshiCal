#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
unset CDPATH

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
cd -- "$script_dir/.."
project_root="$(pwd -P)"

for command in docker curl tar sha256sum flock python3 awk seq sleep stat install mv grep sed date id; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "缺少更新所需命令: $command" >&2
    exit 1
  fi
done
if ! docker compose version >/dev/null 2>&1; then
  echo "需要 Docker Compose v2。" >&2
  exit 1
fi

if [[ "$(id -u)" -eq 0 ]]; then
  lock_file="${OSHICAL_UPDATE_LOCK:-/run/lock/oshical-server-update.lock}"
else
  if [[ -z "${XDG_RUNTIME_DIR:-}" || ! -d "$XDG_RUNTIME_DIR" ]]; then
    echo "非 root 更新必须提供安全的 XDG_RUNTIME_DIR。" >&2
    exit 1
  fi
  lock_file="${OSHICAL_UPDATE_LOCK:-$XDG_RUNTIME_DIR/oshical-server-update.lock}"
fi
exec 9>"$lock_file"
if ! flock -n 9; then
  echo "已有服务器更新正在执行，拒绝并发更新。" >&2
  exit 75
fi

image_repository="ghcr.io/yuetc/oshical-server"
if [[ -z "${OSHICAL_IMAGE:-}" ]]; then
  echo "必须通过 OSHICAL_IMAGE 提供固定摘要镜像引用。" >&2
  exit 1
fi
export OSHICAL_IMAGE
if [[ ! "$OSHICAL_IMAGE" =~ ^ghcr\.io/yuetc/oshical-server@sha256:[0-9a-f]{64}$ ]]; then
  echo "OSHICAL_IMAGE 必须固定为 $image_repository@sha256:<64hex>，拒绝可变标签或其它仓库。" >&2
  exit 1
fi

semver_regex='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
if [[ -n "${OSHICAL_TARGET_VERSION:-}" && ! "$OSHICAL_TARGET_VERSION" =~ $semver_regex ]]; then
  echo "OSHICAL_TARGET_VERSION 不是有效 SemVer: $OSHICAL_TARGET_VERSION" >&2
  exit 1
fi

health_url="${OSHICAL_HEALTH_URL:-http://127.0.0.1:${OSHICAL_PORT:-8787}/api/health}"
if ! python3 - "$health_url" <<'PY'
import ipaddress
import sys
import urllib.parse

url = urllib.parse.urlsplit(sys.argv[1])
if url.scheme not in {"http", "https"}:
    raise SystemExit("health URL must use HTTP or HTTPS")
if url.username is not None or url.password is not None or url.query or url.fragment:
    raise SystemExit("health URL must not contain credentials, query, or fragment")
host = (url.hostname or "").lower()
try:
    address = ipaddress.ip_address(host)
except ValueError:
    if host != "localhost":
        raise SystemExit("health URL must target loopback")
else:
    if not address.is_loopback:
        raise SystemExit("health URL must target loopback")
if url.port is not None and not 1 <= url.port <= 65535:
    raise SystemExit("invalid health URL port")
PY
then
  echo "健康检查地址不安全，只允许本机 HTTP(S)。" >&2
  exit 1
fi

backup_dir="${OSHICAL_BACKUP_DIR:-backups}"
data_dir="${OSHICAL_DATA_DIR:-data}"
if [[ -L "$backup_dir" || ( -e "$backup_dir" && ! -d "$backup_dir" ) ]]; then
  echo "备份目录必须是普通目录，不能是符号链接: $backup_dir" >&2
  exit 1
fi
install -d -m 0700 -- "$backup_dir"
backup_dir="$(cd -- "$backup_dir" && pwd -P)"
case "$backup_dir" in
  "$project_root"/*) ;;
  *)
    echo "备份目录必须位于项目目录内: $backup_dir" >&2
    exit 1
    ;;
esac
backup_mode="$(stat -c '%a' -- "$backup_dir")"
if (( 8#$backup_mode & 8#077 )); then
  echo "备份目录必须禁止组/其他用户访问: $backup_dir" >&2
  exit 1
fi

if [[ -L "$data_dir" || ! -d "$data_dir" ]]; then
  echo "数据目录不存在或不是普通目录，拒绝在未知状态下更新: $data_dir" >&2
  exit 1
fi
data_dir="$(cd -- "$data_dir" && pwd -P)"
case "$data_dir" in
  "$project_root"/*) ;;
  *)
    echo "数据目录必须位于项目目录内: $data_dir" >&2
    exit 1
    ;;
esac
case "$backup_dir/" in
  "$data_dir/"*)
    echo "备份目录不能位于数据目录内，避免备份自包含或回滚污染。" >&2
    exit 1
    ;;
esac
if [[ "$(stat -c '%d' -- "$backup_dir")" != "$(stat -c '%d' -- "${data_dir%/}/..")" ]]; then
  echo "备份目录必须与数据目录的父目录位于同一文件系统，保证回滚目录切换原子可用。" >&2
  exit 1
fi

timestamp="$(date -u +%Y%m%d-%H%M%S)"
backup_file="$backup_dir/oshical-data-$timestamp-$$.tar.gz"

health_version() {
  curl -q --fail --silent --show-error \
    --proto '=http,https' --proto-redir '=http,https' \
    --noproxy '*' --max-time 10 --max-filesize 65536 \
    "$health_url" \
    | python3 -c '
import json
import re
import sys

try:
    raw = sys.stdin.buffer.read(65537)
    if len(raw) > 65536:
        raise ValueError("health response is too large")
    payload = json.loads(raw)
    data = payload.get("data", payload)
    version = data.get("version") if isinstance(data, dict) else None
except (ValueError, TypeError):
    raise SystemExit(1)
if not isinstance(version, str) or not re.fullmatch(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?",
    version,
):
    raise SystemExit(1)
print(version)
'
}

version_is_newer() {
  python3 - "$1" "$2" <<'PY'
import re
import sys

SEMVER = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)

def parse(value):
    match = SEMVER.fullmatch(value)
    if not match:
        raise ValueError("invalid semver")
    core = tuple(int(match.group(index)) for index in range(1, 4))
    prerelease = match.group(4)
    if prerelease is None:
        return core + (1, ())
    parts = []
    for part in prerelease.split("."):
        if part.isdigit():
            parts.append((0, int(part)))
        else:
            parts.append((1, part))
    return core + (0, tuple(parts))

candidate = parse(sys.argv[1])
current = parse(sys.argv[2])
raise SystemExit(0 if candidate > current else 1)
PY
}

previous_version_hint="$(health_version 2>/dev/null || true)"
if [[ ! "$previous_version_hint" =~ $semver_regex ]]; then
  echo "无法读取当前服务器版本，拒绝在未知状态下更新。" >&2
  exit 1
fi
if [[ -n "${OSHICAL_TARGET_VERSION:-}" ]] && ! version_is_newer "$OSHICAL_TARGET_VERSION" "$previous_version_hint"; then
  echo "目标版本必须严格高于当前版本: $previous_version_hint -> $OSHICAL_TARGET_VERSION" >&2
  exit 1
fi

previous_image="$(docker compose images -q oshical 2>/dev/null | sed -n '1p' || true)"
if [[ -z "$previous_image" ]]; then
  echo "未找到当前运行的 oshical 镜像，无法保证失败后可回滚，拒绝更新。" >&2
  exit 1
fi

rollback_image="oshical-server:rollback-$timestamp-$$"
if ! docker tag "$previous_image" "$rollback_image"; then
  echo "无法保存当前镜像标签，拒绝更新。" >&2
  exit 1
fi

wait_for_health() {
  local expected_version="${1:-}"
  local attempt current_version
  for attempt in $(seq 1 30); do
    if current_version="$(health_version 2>/dev/null)" && [[ -n "$current_version" ]]; then
      if [[ -z "$expected_version" || "$current_version" == "$expected_version" ]]; then
        return 0
      fi
    fi
    sleep 2
  done
  return 1
}

start_image() {
  local image="$1"
  OSHICAL_IMAGE="$image" docker compose up -d --pull never --no-deps --force-recreate oshical
}

sha256_file() {
  sha256sum -- "$1" | awk '{print $1}'
}

validate_backup_archive() {
  local archive="$1" expected_sha256="$2" extract_dir="${3:-}"
  python3 - "$archive" "$expected_sha256" "$extract_dir" <<'PY'
import hashlib
import os
import pathlib
import re
import stat
import sys
import tarfile

archive_path, expected_sha256, extract_dir = sys.argv[1:4]
if not re.fullmatch(r"[0-9a-f]{64}", expected_sha256):
    raise SystemExit("invalid backup digest")

archive_fd = os.open(
    archive_path,
    os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
)
archive_metadata = os.fstat(archive_fd)
if not stat.S_ISREG(archive_metadata.st_mode) or archive_metadata.st_nlink != 1:
    os.close(archive_fd)
    raise SystemExit("backup must be a single regular file")
if archive_metadata.st_uid != os.geteuid():
    os.close(archive_fd)
    raise SystemExit("backup is not owned by the updater user")
with os.fdopen(archive_fd, "rb") as handle:
    digest = hashlib.sha256()
    while True:
        chunk = handle.read(1024 * 1024)
        if not chunk:
            break
        digest.update(chunk)
    if digest.hexdigest() != expected_sha256:
        raise SystemExit("backup digest mismatch")
    handle.seek(0)

    with tarfile.open(fileobj=handle, mode="r:gz") as archive:
        members = archive.getmembers()
        if not members:
            raise SystemExit("backup archive is empty")
        if len(members) > 1_000_000:
            raise SystemExit("backup archive contains too many entries")

        seen = {}
        for member in members:
            if "\x00" in member.name:
                raise SystemExit("backup entry contains NUL")
            if not (member.isdir() or member.isreg()):
                raise SystemExit(f"unsupported archive entry type: {member.name}")
            if member.mode & 0o6000:
                raise SystemExit(f"setuid/setgid archive entry: {member.name}")

            pure = pathlib.PurePosixPath(member.name)
            if pure.is_absolute() or ".." in pure.parts:
                raise SystemExit(f"unsafe archive path: {member.name}")
            parts = tuple(part for part in pure.parts if part not in ("", "."))
            normalized = "/".join(parts)
            if not normalized:
                if not member.isdir():
                    raise SystemExit("archive root entry must be a directory")
                normalized = "."
            entry_type = "dir" if member.isdir() else "file"
            if normalized in seen:
                raise SystemExit(f"duplicate archive entry: {member.name}")

            parent = pathlib.PurePosixPath(normalized).parent
            while str(parent) not in ("", "."):
                if seen.get(str(parent)) == "file":
                    raise SystemExit(f"archive file used as parent directory: {parent}")
                parent = parent.parent
            seen[normalized] = entry_type

        if extract_dir:
            archive.extractall(path=extract_dir, members=members, numeric_owner=True)
PY
}

restore_data_backup() {
  local restore_dir="${data_dir%/}.restore-$timestamp-$$"
  local failed_data_dir="$backup_dir/oshical-data-failed-$timestamp-$$"

  if [[ ! -f "$backup_file" || -L "$backup_file" ]]; then
    echo "更新前数据备份不存在或不是普通文件，无法自动恢复: $backup_file" >&2
    return 1
  fi
  if [[ -e "$restore_dir" || -L "$restore_dir" ]]; then
    echo "恢复临时目录已存在，拒绝覆盖: $restore_dir" >&2
    return 1
  fi
  if [[ -e "$failed_data_dir" || -L "$failed_data_dir" ]]; then
    echo "故障现场目录已存在，拒绝覆盖: $failed_data_dir" >&2
    return 1
  fi

  mkdir -m 0700 -- "$restore_dir"
  if ! validate_backup_archive "$backup_file" "$backup_sha256" "$restore_dir"; then
    echo "数据备份完整性或路径校验失败: $backup_file" >&2
    rm -rf -- "$restore_dir"
    return 1
  fi

  if [[ ! -d "$data_dir" || -L "$data_dir" ]]; then
    echo "当前数据目录状态异常，无法保留故障现场: $data_dir" >&2
    rm -rf -- "$restore_dir"
    return 1
  fi
  if ! mv -- "$data_dir" "$failed_data_dir"; then
    echo "保留故障数据目录失败: $failed_data_dir" >&2
    rm -rf -- "$restore_dir"
    return 1
  fi
  if ! mv -- "$restore_dir" "$data_dir"; then
    echo "恢复数据目录失败，尝试恢复故障现场: $failed_data_dir" >&2
    mv -- "$failed_data_dir" "$data_dir" || true
    return 1
  fi

  echo "已从更新前备份恢复数据: $backup_file"
  echo "更新失败时的数据现场已保留: $failed_data_dir"
}

restart_previous_without_restore() {
  local restored_version=""

  if ! docker compose stop oshical >/dev/null 2>&1; then
    echo "无法停止失败流程中的容器，拒绝继续操作。" >&2
    return 1
  fi
  if ! start_image "$rollback_image"; then
    echo "恢复上一镜像失败: $rollback_image" >&2
    return 1
  fi
  if ! wait_for_health; then
    echo "上一镜像启动后健康检查失败: $rollback_image" >&2
    return 1
  fi

  restored_version="$(health_version 2>/dev/null || true)"
  if [[ "$restored_version" != "$previous_version_hint" ]]; then
    echo "上一版本健康检查通过，但版本号不一致：期望 $previous_version_hint，实际 ${restored_version:-unknown}" >&2
    return 1
  fi
  transaction_active=false
  echo "已恢复上一版本: $restored_version"
}

rollback_after_failure() {
  local reason="$1"
  local failed_log="$backup_dir/oshical-failed-container-$timestamp-$$.log"
  local restored_version=""
  rollback_in_progress=true

  echo "$reason" >&2
  docker logs --tail 2000 oshical >"$failed_log" 2>&1 || true
  echo "新容器日志已保留: $failed_log"

  if ! docker compose stop oshical >/dev/null 2>&1; then
    echo "无法停止新容器，拒绝在容器运行期间替换数据目录。" >&2
    return 1
  fi
  if ! restore_data_backup; then
    echo "自动恢复数据失败；已停止容器，现场和备份均保留。请人工恢复后再启动服务。" >&2
    return 1
  fi
  if ! start_image "$rollback_image"; then
    echo "数据已恢复，但上一镜像启动失败: $rollback_image" >&2
    return 1
  fi
  if ! wait_for_health; then
    echo "数据已恢复，但上一镜像健康检查失败: $rollback_image" >&2
    return 1
  fi

  restored_version="$(health_version 2>/dev/null || true)"
  if [[ "$restored_version" != "$previous_version_hint" ]]; then
    echo "旧版本健康检查通过，但版本号不一致：期望 $previous_version_hint，实际 ${restored_version:-unknown}" >&2
    return 1
  fi

  transaction_active=false
  rollback_in_progress=false
  echo "回滚成功，旧版本已通过健康检查: $restored_version"
}

transaction_active=false
update_committed=false
rollback_in_progress=false

cleanup_on_exit() {
  local original_status=$?

  if [[ "$transaction_active" == true && "$update_committed" == false && "$rollback_in_progress" == false ]]; then
    echo "更新流程异常中断，正在恢复上一版本。" >&2
    if [[ -n "${backup_sha256:-}" && -f "$backup_file" ]]; then
      rollback_after_failure "更新流程异常中断，恢复更新前数据与上一镜像。" || true
    else
      restart_previous_without_restore || true
    fi
  fi
  return "$original_status"
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

if ! docker compose pull oshical; then
  echo "拉取新镜像失败，保持当前服务不变。" >&2
  exit 1
fi

new_image="$(docker image inspect --format '{{.Id}}' "$OSHICAL_IMAGE" 2>/dev/null || true)"
if [[ -z "$new_image" ]]; then
  echo "无法读取新镜像 ID，保持当前服务不变。" >&2
  exit 1
fi
if ! docker image inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$OSHICAL_IMAGE" \
  | grep -Fqx -- "$OSHICAL_IMAGE"; then
  echo "新镜像的仓库摘要与请求不一致，拒绝启动: $OSHICAL_IMAGE" >&2
  exit 1
fi
if [[ "$previous_image" == "$new_image" ]]; then
  echo "当前已是最新镜像: $OSHICAL_IMAGE"
  exit 0
fi

echo "停止服务以生成一致性数据备份..."
if ! docker compose stop oshical; then
  echo "停止服务失败，未修改数据。" >&2
  exit 1
fi
transaction_active=true

if ! tar --one-file-system -C "$data_dir" -czf "$backup_file" .; then
  echo "数据备份失败，恢复原服务。" >&2
  restart_previous_without_restore
  exit 1
fi
backup_sha256="$(sha256_file "$backup_file")"
if ! validate_backup_archive "$backup_file" "$backup_sha256"; then
  echo "数据备份完整性或路径校验失败，恢复原服务。" >&2
  rm -f -- "$backup_file"
  restart_previous_without_restore
  exit 1
fi
echo "数据备份完成: $backup_file"

if ! start_image "$OSHICAL_IMAGE"; then
  if ! rollback_after_failure "启动新镜像失败，恢复更新前数据与上一镜像。"; then
    exit 1
  fi
  exit 1
fi

if ! wait_for_health "${OSHICAL_TARGET_VERSION:-}"; then
  if ! rollback_after_failure "新版本健康检查或目标版本校验失败，恢复更新前数据与上一镜像。"; then
    exit 1
  fi
  exit 1
fi

refreshed_version="$(health_version 2>/dev/null || true)"
if [[ -n "${OSHICAL_TARGET_VERSION:-}" && "$refreshed_version" != "$OSHICAL_TARGET_VERSION" ]]; then
  if ! rollback_after_failure "健康检查版本 $refreshed_version 与目标版本 $OSHICAL_TARGET_VERSION 不一致。"; then
    exit 1
  fi
  exit 1
fi

update_committed=true
transaction_active=false
echo "服务更新成功（不可变摘要）: $OSHICAL_IMAGE"
echo "版本校验: ${refreshed_version:-unknown}"
echo "健康检查: $health_url"
