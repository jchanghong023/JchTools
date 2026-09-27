"""回归：源码 OCR 清单必须指向已固定的可下载 worker 字节."""

from __future__ import annotations

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "resources" / "snap-ocr-assets.json"
WORKER_URL = (
    "https://github.com/jchanghong023/JchTools/releases/download/optional-components-v0.1.0/snap-ocr-worker.exe"
)


def main() -> int:
    data = json.loads(MANIFEST.read_text(encoding="utf-8"))
    workers = data["workers"]
    assert len(workers) == 1, "OCR worker 清单必须恰好有一项"
    worker = workers[0]
    assert worker["id"] == "snap-ocr-worker"
    assert worker["status"] == "ok", "发布后的源码清单不得保留 pending-build"
    assert worker["url"] == WORKER_URL
    assert worker["size_bytes"] > 0
    assert re.fullmatch(r"[0-9a-f]{64}", worker["sha256"])
    print("PASS OCR worker 清单已固定发布字节")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
