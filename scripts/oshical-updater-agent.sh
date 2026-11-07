#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
unset CDPATH

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
  echo "更新代理必须以 root 运行。" >&2
  exit 1
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
cd -- "$script_dir/.."

for command in flock curl python3 mkdir mktemp install stat date id; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "缺少更新代理所需命令: $command" >&2
    exit 1
  fi
done
exec 8>"/run/lock/oshical-updater-agent.lock"
if ! flock -n 8; then
  echo "已有更新代理实例正在执行，拒绝并发运行。" >&2
  exit 75
fi

update_dir="${OSHICAL_UPDATE_DIR:-$PWD/data/system}"
request_file="$update_dir/update-request.env"
status_file="$update_dir/server-update-status.json"
heartbeat_file="$update_dir/updater-heartbeat.json"
image_repository="ghcr.io/yuetc/oshical-server"
mkdir -p -- "$update_dir"

if ! command -v python3 >/dev/null 2>&1; then
  echo "未找到 python3，无法安全处理更新控制文件。" >&2
  exit 1
fi

# All fixed control paths are opened relative to pinned directory descriptors
# with O_NOFOLLOW. The application volume is treated as untrusted input: it may
# provide requests, but it cannot redirect root writes through symlinked paths.
safe_replace_file() {
  local target="$1"
  python3 - "$target" 3<&0 <<'PY'
import os
import secrets
import sys

MAX_BYTES = 64 * 1024

def open_parent(path):
    if not path.startswith(os.sep):
        raise RuntimeError("control path must be absolute")
    parts = [part for part in path.split(os.sep) if part not in ("", ".")]
    if not parts or ".." in parts:
        raise RuntimeError("invalid control path")
    fd = os.open(os.sep, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in parts[:-1]:
            next_fd = os.open(
                part,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                dir_fd=fd,
            )
            os.close(fd)
            fd = next_fd
    except BaseException:
        os.close(fd)
        raise
    return fd, parts[-1]

data = bytearray()
while True:
    chunk = os.read(3, 65536)
    if not chunk:
        break
    data.extend(chunk)
    if len(data) > MAX_BYTES:
        raise RuntimeError("control file exceeds size limit")

parent_fd, name = open_parent(os.path.abspath(sys.argv[1]))
temporary = f".oshical-{os.getpid()}-{secrets.token_hex(16)}"
temporary_fd = -1
try:
    temporary_fd = os.open(
        temporary,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
        0o600,
        dir_fd=parent_fd,
    )
    view = memoryview(data)
    while view:
        written = os.write(temporary_fd, view)
        if written <= 0:
            raise RuntimeError("control file write made no progress")
        view = view[written:]
    os.fsync(temporary_fd)
    os.close(temporary_fd)
    temporary_fd = -1
    os.replace(temporary, name, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
    os.fsync(parent_fd)
except BaseException:
    if temporary_fd >= 0:
        os.close(temporary_fd)
    try:
        os.unlink(temporary, dir_fd=parent_fd)
    except FileNotFoundError:
        pass
    raise
finally:
    os.close(parent_fd)
PY
}

safe_append_file() {
  local target="$1"
  python3 - "$target" 3<&0 <<'PY'
import os
import stat
import sys

def open_parent(path):
    if not path.startswith(os.sep):
        raise RuntimeError("control path must be absolute")
    parts = [part for part in path.split(os.sep) if part not in ("", ".")]
    if not parts or ".." in parts:
        raise RuntimeError("invalid control path")
    fd = os.open(os.sep, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in parts[:-1]:
            next_fd = os.open(
                part,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                dir_fd=fd,
            )
            os.close(fd)
            fd = next_fd
    except BaseException:
        os.close(fd)
        raise
    return fd, parts[-1]

def open_regular_for_append(parent_fd, name):
    try:
        return os.open(
            name,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_APPEND | os.O_NOFOLLOW | os.O_CLOEXEC,
            0o600,
            dir_fd=parent_fd,
        )
    except FileExistsError:
        pass

    path_fd = os.open(
        name,
        os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC,
        dir_fd=parent_fd,
    )
    try:
        expected = os.fstat(path_fd)
        if not stat.S_ISREG(expected.st_mode) or expected.st_nlink != 1:
            raise RuntimeError("refusing non-regular or multiply-linked updater log")
        if expected.st_uid != os.geteuid():
            raise RuntimeError("refusing updater log not owned by updater user")
        fd = os.open(
            f"/proc/self/fd/{path_fd}",
            os.O_WRONLY | os.O_APPEND | os.O_CLOEXEC,
        )
        actual = os.fstat(fd)
        if (actual.st_dev, actual.st_ino) != (expected.st_dev, expected.st_ino):
            os.close(fd)
            raise RuntimeError("updater log changed while opening")
        return fd
    finally:
        os.close(path_fd)

parent_fd, name = open_parent(os.path.abspath(sys.argv[1]))
fd = -1
try:
    fd = open_regular_for_append(parent_fd, name)
    metadata = os.fstat(fd)
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
        raise RuntimeError("refusing non-regular or multiply-linked updater log")
    if metadata.st_uid != os.geteuid():
        raise RuntimeError("refusing updater log not owned by updater user")
    while True:
        chunk = os.read(3, 65536)
        if not chunk:
            break
        view = memoryview(chunk)
        while view:
            written = os.write(fd, view)
            if written <= 0:
                raise RuntimeError("updater log write made no progress")
            view = view[written:]
    os.fsync(fd)
finally:
    if fd >= 0:
        os.close(fd)
    os.close(parent_fd)
PY
}

safe_remove_file() {
  local target="$1"
  python3 - "$target" <<'PY'
import os
import sys

path = os.path.abspath(sys.argv[1])
parts = [part for part in path.split(os.sep) if part not in ("", ".")]
if not parts or ".." in parts:
    raise RuntimeError("invalid control path")
fd = os.open(os.sep, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
try:
    for part in parts[:-1]:
        next_fd = os.open(
            part,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
            dir_fd=fd,
        )
        os.close(fd)
        fd = next_fd
    try:
        os.unlink(parts[-1], dir_fd=fd)
    except FileNotFoundError:
        pass
    os.fsync(fd)
finally:
    os.close(fd)
PY
}

write_status() {
  local status="$1" phase="$2" message="$3" error="${4:-}" target="${5:-}"
  python3 - "$status" "$phase" "$message" "$error" "$target" <<'PY' | safe_replace_file "$status_file"
import datetime
import json
import sys

status, phase, message, error, target = sys.argv[1:6]
payload = {
    "status": status,
    "phase": phase,
    "message": message,
    "targetVersion": target or None,
    "updatedAt": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "error": error or None,
}
print(json.dumps(payload, ensure_ascii=False, separators=(",", ":")), flush=True)
PY
}

write_heartbeat() {
  python3 - <<'PY' | safe_replace_file "$heartbeat_file"
import datetime
import json

print(
    json.dumps(
        {"updatedAt": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")},
        separators=(",", ":"),
    ),
    flush=True,
)
PY
}

write_heartbeat

if [[ ! -e "$request_file" ]]; then
  exit 0
fi

signing_key_file="${OSHICAL_UPDATE_HMAC_KEY_FILE:-}"
if [[ -z "$signing_key_file" && -n "${CREDENTIALS_DIRECTORY:-}" ]]; then
  signing_key_file="$CREDENTIALS_DIRECTORY/update-hmac-key"
fi
if [[ -z "$signing_key_file" ]]; then
  signing_key_file="/etc/oshical/updater-hmac.key"
fi

fields_file="$(mktemp "${TMPDIR:-/tmp}/oshical-update-fields.XXXXXX")"
reason_file="$(mktemp "${TMPDIR:-/tmp}/oshical-update-reason.XXXXXX")"
heartbeat_pid=""
stop_heartbeat_loop() {
  if [[ -n "$heartbeat_pid" ]]; then
    kill "$heartbeat_pid" 2>/dev/null || true
    wait "$heartbeat_pid" 2>/dev/null || true
    heartbeat_pid=""
  fi
}
trap 'stop_heartbeat_loop; rm -f -- "$fields_file" "$reason_file"' EXIT

if ! python3 - "$request_file" "$signing_key_file" "$image_repository" >"$fields_file" 2>"$reason_file" <<'PY'
import hashlib
import hmac
import os
import re
import stat
import sys

request_path, key_path, image_repository = sys.argv[1:4]
MAX_REQUEST_BYTES = 16 * 1024
SEMVER = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)
UUID4 = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-4[0-9a-fA-F]{3}-"
    r"[89aAbB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$"
)
DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
SIGNATURE = re.compile(r"^[0-9a-f]{64}$")
HHMM = re.compile(r"^(?:[01][0-9]|2[0-3]):[0-5][0-9]$")
ALLOWED = {
    "request_id",
    "target_version",
    "target_digest",
    "signature",
    "force",
    "window_start",
    "window_end",
    "requested_at",
}
REQUIRED = ALLOWED - {"requested_at"}

def open_parent(path):
    if not path.startswith(os.sep):
        raise ValueError("request path must be absolute")
    parts = [part for part in path.split(os.sep) if part not in ("", ".")]
    if not parts or ".." in parts:
        raise ValueError("invalid request path")
    fd = os.open(os.sep, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in parts[:-1]:
            next_fd = os.open(
                part,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                dir_fd=fd,
            )
            os.close(fd)
            fd = next_fd
    except BaseException:
        os.close(fd)
        raise
    return fd, parts[-1]

def read_key(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise ValueError("HMAC key must be a single regular file")
        if metadata.st_uid != os.geteuid() or metadata.st_mode & 0o077:
            raise ValueError("HMAC key must be owned by updater user with mode 0400 or 0600")
        raw = bytearray()
        while True:
            chunk = os.read(fd, 4096)
            if not chunk:
                break
            raw.extend(chunk)
            if len(raw) > 4096:
                raise ValueError("HMAC key file is too large")
        return bytes(raw)
    finally:
        os.close(fd)

parent_fd, name = open_parent(os.path.abspath(request_path))
request_fd = -1
path_fd = -1
try:
    path_fd = os.open(
        name,
        os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC,
        dir_fd=parent_fd,
    )
    expected = os.fstat(path_fd)
    if not stat.S_ISREG(expected.st_mode) or expected.st_nlink != 1:
        raise ValueError("request must be a single regular file")
    if expected.st_size > MAX_REQUEST_BYTES:
        raise ValueError("request file is too large")
    request_fd = os.open(
        f"/proc/self/fd/{path_fd}",
        os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC,
    )
    actual = os.fstat(request_fd)
    if (actual.st_dev, actual.st_ino) != (expected.st_dev, expected.st_ino):
        raise ValueError("request file changed while opening")
    if not stat.S_ISREG(actual.st_mode) or actual.st_nlink != 1:
        raise ValueError("request must be a single regular file")
    data = bytearray()
    while True:
        chunk = os.read(request_fd, 65536)
        if not chunk:
            break
        data.extend(chunk)
        if len(data) > MAX_REQUEST_BYTES:
            raise ValueError("request file is too large")
finally:
    if request_fd >= 0:
        os.close(request_fd)
    if path_fd >= 0:
        os.close(path_fd)
    os.close(parent_fd)

try:
    text = data.decode("utf-8")
except UnicodeDecodeError as error:
    raise ValueError("request is not UTF-8") from error
if "\x00" in text or "\r" in text:
    raise ValueError("request contains forbidden control characters")

values = {}
for line in text.split("\n"):
    if not line:
        continue
    if "=" not in line:
        raise ValueError("request line is missing '='")
    key, value = line.split("=", 1)
    if key not in ALLOWED or key in values:
        raise ValueError("request contains an unknown or duplicate field")
    if any(ord(char) < 0x20 or ord(char) == 0x7F for char in value):
        raise ValueError("request field contains control characters")
    values[key] = value

if not REQUIRED.issubset(values):
    raise ValueError("request is missing required fields")
if not UUID4.fullmatch(values["request_id"]):
    raise ValueError("invalid request id")
if not SEMVER.fullmatch(values["target_version"]):
    raise ValueError("invalid target version")
if not DIGEST.fullmatch(values["target_digest"]):
    raise ValueError("invalid target digest")
if not SIGNATURE.fullmatch(values["signature"]):
    raise ValueError("invalid update signature")
if values["force"] not in {"true", "false"}:
    raise ValueError("invalid force flag")
if not HHMM.fullmatch(values["window_start"]) or not HHMM.fullmatch(values["window_end"]):
    raise ValueError("invalid update window")

def fail(code, detail):
    print(f"REASON:{code}:{detail}", file=sys.stderr, flush=True)
    raise SystemExit(3)


try:
    key_raw = read_key(key_path)
except OSError as error:
    fail("key_unreadable", error.strerror or error.__class__.__name__)
except ValueError as error:
    fail("key_invalid", str(error))
try:
    key_text = key_raw.decode("ascii").strip()
except UnicodeDecodeError:
    fail("key_invalid", "key is not ASCII")
if not SIGNATURE.fullmatch(key_text):
    fail("key_invalid", "key must be exactly 64 lowercase hex characters")
key = bytes.fromhex(key_text)
payload = (
    f"server\n{image_repository}\n"
    f"{values['target_version']}\n{values['target_digest']}"
).encode("ascii")
expected = hmac.new(key, payload, hashlib.sha256).hexdigest()
if not hmac.compare_digest(expected, values["signature"]):
    fail("signature_mismatch", "signature does not match this host key")

for field in (
    "request_id",
    "target_version",
    "target_digest",
    "force",
    "window_start",
    "window_end",
):
    sys.stdout.buffer.write(values[field].encode("utf-8") + b"\0")
PY
then
  reason="$(grep -o 'REASON:[a-z_]*' "$reason_file" | head -n 1 || true)"
  detail="$(grep -o 'REASON:[^:]*:.*' "$reason_file" | head -n 1 || true)"
  detail="${detail#*:*:}"
  case "$reason" in
    REASON:key_unreadable*)
      message="更新代理的 HMAC 密钥缺失或不可读：$signing_key_file"
      ;;
    REASON:key_invalid*)
      message="更新代理的 HMAC 密钥无效：需 64 位小写十六进制、root 所有、权限 0400/0600"
      ;;
    REASON:signature_mismatch*)
      message="宿主机 HMAC 密钥与发布清单签名不匹配，更新被拒绝"
      ;;
    *)
      message="更新请求未通过结构或签名校验"
      ;;
  esac
  echo "$message" >&2
  write_status "failed" "validation" "$message" "${detail:-$reason}"
  safe_remove_file "$request_file" || true
  exit 1
fi

mapfile -d '' -t request_fields <"$fields_file"
if [[ "${#request_fields[@]}" -ne 6 ]]; then
  echo "更新请求字段数量无效。" >&2
  write_status "failed" "validation" "更新请求字段数量无效" "invalid update request field count"
  safe_remove_file "$request_file" || true
  exit 1
fi

request_id="${request_fields[0]}"
target_version="${request_fields[1]}"
target_digest="${request_fields[2]}"
force="${request_fields[3]}"
window_start="${request_fields[4]}"
window_end="${request_fields[5]}"
unset request_fields

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
  write_status "failed" "validation" "健康检查地址不安全，只允许本机 HTTP(S)" "unsafe health URL" "$target_version"
  safe_remove_file "$request_file" || true
  exit 1
fi

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

state_dir="${OSHICAL_UPDATE_STATE_DIR:-${STATE_DIRECTORY:-/var/lib/oshical-updater}}"
installed_version_file="$state_dir/installed-version"
if [[ -L "$state_dir" || ( -e "$state_dir" && ! -d "$state_dir" ) ]]; then
  write_status "failed" "validation" "更新状态目录不是普通目录" "unsafe updater state directory" "$target_version"
  safe_remove_file "$request_file" || true
  exit 1
fi
install -d -m 0700 -- "$state_dir"
state_owner="$(stat -c '%u' -- "$state_dir")"
state_mode="$(stat -c '%a' -- "$state_dir")"
if [[ "$state_owner" != "$(id -u)" ]] || (( 8#$state_mode & 8#077 )); then
  write_status "failed" "validation" "更新状态目录权限不安全" "unsafe updater state directory" "$target_version"
  safe_remove_file "$request_file" || true
  exit 1
fi

read_installed_version() {
  python3 - "$installed_version_file" <<'PY'
import os
import stat
import sys

path = os.path.abspath(sys.argv[1])
parts = [part for part in path.split(os.sep) if part not in ("", ".")]
if not parts or ".." in parts:
    raise SystemExit("invalid state path")
parent_fd = os.open(os.sep, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
try:
    for part in parts[:-1]:
        next_fd = os.open(
            part,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
            dir_fd=parent_fd,
        )
        os.close(parent_fd)
        parent_fd = next_fd
    try:
        fd = os.open(
            parts[-1],
            os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
            dir_fd=parent_fd,
        )
    except FileNotFoundError:
        raise SystemExit(0)
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise SystemExit("installed-version must be a single regular file")
        if metadata.st_uid != os.geteuid() or metadata.st_size > 128:
            raise SystemExit("installed-version has unsafe ownership or size")
        raw = os.read(fd, 129)
        if len(raw) > 128:
            raise SystemExit("installed-version is too large")
    finally:
        os.close(fd)
finally:
    os.close(parent_fd)
value = raw.decode("ascii").strip()
if value:
    print(value)
PY
}

record_installed_version() {
  printf '%s
' "$1" | safe_replace_file "$installed_version_file"
}

# installed-version 状态校验引用了 $semver_regex，但脚本从未定义它。
# 在 set -u 下第一次真正处理请求就会以 unbound variable 退出，
# 表现为状态停在 requested/queued、journal 里只有这一行错误。
semver_regex='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
if ! current_version="$(health_version 2>/dev/null)"; then
  write_status "failed" "failed" "无法读取当前服务器版本，稍后重试" "server health request failed" "$target_version"
  exit 1
fi
if ! installed_version="$(read_installed_version 2>/dev/null)"; then
  write_status "failed" "validation" "已安装版本状态损坏，拒绝继续更新" "invalid installed-version state" "$target_version"
  safe_remove_file "$request_file" || true
  exit 1
fi
if [[ -n "$installed_version" && ! "$installed_version" =~ $semver_regex ]]; then
  write_status "failed" "validation" "已安装版本状态格式无效，拒绝继续更新" "invalid installed-version state" "$target_version"
  safe_remove_file "$request_file" || true
  exit 1
fi
if [[ -n "$installed_version" && "$current_version" != "$installed_version" ]]; then
  if version_is_newer "$current_version" "$installed_version"; then
    if ! record_installed_version "$current_version"; then
      write_status "failed" "failed" "无法持久化已安装版本状态" "failed to write installed-version state" "$target_version"
      safe_remove_file "$request_file" || true
      exit 1
    fi
    installed_version="$current_version"
  else
    write_status "failed" "validation" "检测到版本低于 root 记录的已安装版本，拒绝静默降级" "running $current_version is below recorded $installed_version" "$target_version"
    safe_remove_file "$request_file" || true
    exit 1
  fi
elif [[ -z "$installed_version" ]]; then
  if ! record_installed_version "$current_version"; then
    write_status "failed" "failed" "无法持久化已安装版本状态" "failed to write installed-version state" "$target_version"
    safe_remove_file "$request_file" || true
    exit 1
  fi
  installed_version="$current_version"
fi
effective_current_version="$current_version"
if version_is_newer "$installed_version" "$effective_current_version"; then
  effective_current_version="$installed_version"
fi
if [[ "$effective_current_version" == "$target_version" ]]; then
  write_status "succeeded" "complete" "服务器已经是最新版本" "" "$target_version"
  safe_remove_file "$request_file" || true
  exit 0
fi
if ! version_is_newer "$target_version" "$effective_current_version"; then
  write_status "failed" "validation" "目标版本低于已安装版本，拒绝降级" "downgrade rejected: $effective_current_version -> $target_version" "$target_version"
  safe_remove_file "$request_file" || true
  exit 1
fi

if [[ "$force" != "true" ]] && ! in_window; then
  write_status "waiting_window" "waiting_window" "等待自动更新时间段" "" "$target_version"
  exit 0
fi

server_image="$image_repository@$target_digest"
pull_message="正在拉取固定摘要服务器镜像 $target_digest"
write_status "running" "pulling" "$pull_message" "" "$target_version"

# A pull, backup, restart, and health check can exceed the 90-second online
# window. Keep refreshing the heartbeat while the long-running update works.
(
  while true; do
    sleep 30
    write_heartbeat || exit 0
  done
) &
heartbeat_pid=$!

if OSHICAL_IMAGE="$server_image" \
  OSHICAL_TARGET_VERSION="$target_version" \
  /bin/bash ./scripts/update-server.sh 2>&1 | safe_append_file "$update_dir/updater.log"; then
  if ! refreshed_version="$(health_version 2>/dev/null)"; then
    write_status "failed" "failed" "更新后无法读取服务器版本，已按失败处理" "server health request failed after update" "$target_version"
    safe_remove_file "$request_file" || true
    exit 1
  fi
  if [[ "$refreshed_version" != "$target_version" ]]; then
    write_status "failed" "failed" "健康检查版本与目标版本不一致，已按失败处理" "expected $target_version, got ${refreshed_version:-unknown}" "$target_version"
    safe_remove_file "$request_file" || true
    exit 1
  fi
  if ! record_installed_version "$target_version"; then
    write_status "failed" "failed" "服务器已更新，但无法持久化已安装版本状态" "failed to write installed-version state after update" "$target_version"
    safe_remove_file "$request_file" || true
    exit 1
  fi
  write_status "succeeded" "complete" "服务器更新成功（固定摘要且签名已验证）" "" "$refreshed_version"
  safe_remove_file "$request_file" || true
  exit 0
fi

write_status "failed" "failed" "服务器更新失败，已尝试回滚" "请检查 updater.log 和容器日志" "$target_version"
safe_remove_file "$request_file" || true
exit 1
