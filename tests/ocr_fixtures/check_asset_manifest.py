"""回归：源码 OCR 清单必须指向已固定的可下载 worker 字节.

推理组件（xberg.exe、O-07 截图模型集、onnxruntime）不再由本清单下载：
识别由 Xberg 推理组件包承接（XB-10，条目待发布 tag 落定后接入），本检查
只钉 worker 发布字节。
"""

from __future__ import annotations

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "resources" / "snap-ocr-assets.json"
WORKER_URL = (
    "https://github.com/jchanghong023/JchTools/releases/download/optional-components-v0.1.1/snap-ocr-worker.exe"
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
    asset_ids = {asset["id"] for asset in data["assets"]}
    assert "noto-sans-mono-cjk-sc" in asset_ids, "结果窗字体条目必须保留（O-21）"
    inference = data.get("xberg_inference")
    if inference is not None:
        # XB-09：推理组件包条目接入后必须钉定发布 zip 与成员摘要。
        assert re.fullmatch(r"[0-9a-f]{64}", inference["sha256"]), "推理组件包必须钉定归档 SHA-256"
        assert inference["size_bytes"] > 0
        assert inference["members"], "推理组件包必须带成员清单"
        for member in inference["members"]:
            assert re.fullmatch(r"[0-9a-f]{64}", member["sha256"])
            assert member["install_path"].startswith(
                f"xberg-inference/{inference['tag']}/"
            ), f"成员必须安装在 xberg-inference/{inference['tag']}/ 下：{member['install_path']}"
    print("PASS OCR worker 清单已固定发布字节")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
