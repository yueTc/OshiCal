#!/usr/bin/env python3
"""校验仓库 Secret 与宿主机 HMAC 密钥是否一致。

用法：
    UPDATE_SIGNING_KEY=<secret> python3 scripts/verify-update-key.py <latest-server.json>

清单里的 signature 是发布 workflow 用 OSHICAL_SERVER_UPDATE_HMAC_KEY 对
`server\\n<镜像仓库>\\n<版本>\\n<镜像摘要>` 计算的 HMAC-SHA256。宿主机用同一把密钥
重算并比对；所以「用 Secret 重算的结果 == 清单 signature」等价于「Secret == 宿主机密钥」。

退出码：0 一致，1 不一致，2 用法或清单错误。
"""

import hashlib
import hmac
import json
import os
import re
import sys

IMAGE_REPOSITORY = "ghcr.io/yuetc/oshical-server"
SIGNATURE = re.compile(r"^[0-9a-f]{64}$")
DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("用法: verify-update-key.py <latest-server.json>", file=sys.stderr)
        return 2

    try:
        with open(argv[1], encoding="utf-8") as handle:
            manifest = json.load(handle)
    except (OSError, ValueError) as error:
        print(f"读取清单失败: {error}", file=sys.stderr)
        return 2

    version = manifest.get("version")
    digest = manifest.get("imageDigest")
    signature = manifest.get("signature")
    if not isinstance(version, str) or not isinstance(digest, str) or not isinstance(signature, str):
        print("清单缺少 version / imageDigest / signature", file=sys.stderr)
        return 2
    if not DIGEST.fullmatch(digest) or not SIGNATURE.fullmatch(signature):
        print("清单字段格式不合法", file=sys.stderr)
        return 2

    key_text = os.environ.get("UPDATE_SIGNING_KEY", "").strip()
    if not SIGNATURE.fullmatch(key_text):
        print(
            "Secret 不是 64 位小写十六进制（长度 %d）：检查是否粘贴了旧值或多余字符"
            % len(key_text),
            file=sys.stderr,
        )
        return 1

    payload = f"server\n{IMAGE_REPOSITORY}\n{version}\n{digest}".encode("ascii")
    computed = hmac.new(bytes.fromhex(key_text), payload, hashlib.sha256).hexdigest()
    print(f"manifest : {version} {digest}")
    print(f"published: {signature}")
    print(f"computed : {computed}")
    if hmac.compare_digest(computed, signature):
        print("MATCH: Secret 与发布签名一致，宿主机可以恢复强制验签")
        return 0
    print("MISMATCH: Secret 不是签发该清单的密钥，宿主机仍会落到未验签放行")
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
