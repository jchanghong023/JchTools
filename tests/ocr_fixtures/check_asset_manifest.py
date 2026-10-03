"""回归：源码 OCR 清单的 worker 条目与推理组件成员结构完整性.

推理组件（xberg.exe、O-07 截图模型集、onnxruntime）不再由本清单下载：
识别由 Xberg 推理组件包承接（XB-10）。worker 按 XB-25 随主包交付：仓库清单
保持 pending-build 构建期占位，package-windows.ps1 打包时用本次构建的真实
大小与 SHA-256 回填 staged 清单并嵌入 EXE——仓库清单不再 pin 某个已发布
release 的字节（源码演进后本地构建必然与其失配，旧口径使离线打包失败、
在线打包嵌入旧清单交付新 worker，运行期校验拒绝服务）。
"""

from __future__ import annotations

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "resources" / "snap-ocr-assets.json"
WORKER_URL = (
    "https://github.com/jchanghong023/JchTools/releases/download/optional-components-v0.1.2/snap-ocr-worker.exe"
)


def main() -> int:
    data = json.loads(MANIFEST.read_text(encoding="utf-8"))
    workers = data["workers"]
    assert len(workers) == 1, "OCR worker 清单必须恰好有一项"
    worker = workers[0]
    assert worker["id"] == "snap-ocr-worker"
    assert worker["url"] == WORKER_URL
    if worker["status"] == "pending-build":
        # 构建期占位：size=0 且 sha256=PENDING-BUILD，真实字节由打包阶段回填
        # （src/snap_ocr_assets.rs 的 load_manifest 同口径校验）。
        assert worker["size_bytes"] == 0, "pending-build 占位的 size_bytes 必须为 0"
        assert worker["sha256"] == "PENDING-BUILD", (
            "pending-build 占位的 sha256 必须是 PENDING-BUILD"
        )
    else:
        assert worker["status"] == "ok", f"worker 状态无效：{worker['status']}"
        assert worker["size_bytes"] > 0
        assert re.fullmatch(r"[0-9a-f]{64}", worker["sha256"])
    asset_ids = {asset["id"] for asset in data["assets"]}
    assert "noto-sans-mono-cjk-sc" in asset_ids, "结果窗字体条目必须保留（O-21）"
    inference = data.get("xberg_inference")
    if inference is not None:
        # XB-09：推理组件包条目必须钉定发布 zip 与成员摘要。
        assert re.fullmatch(r"[0-9a-f]{64}", inference["sha256"]), "推理组件包必须钉定归档 SHA-256"
        assert inference["size_bytes"] > 0
        assert inference["members"], "推理组件包必须带成员清单"
        for member in inference["members"]:
            assert re.fullmatch(r"[0-9a-f]{64}", member["sha256"])
            assert member["install_path"].startswith(
                f"xberg-inference/{inference['tag']}/"
            ), f"成员必须安装在 xberg-inference/{inference['tag']}/ 下：{member['install_path']}"
        # P2-9 回归：成员名必须与 Xberg 发布物实际布局一致——许可文件在
        # models/paddleocr-onnx-models-LICENSE.txt，不存在
        # models/snapshot-ocr/LICENSE-paddleocr.txt；占着错误名字会让标准
        # run54.1 目录被截图就绪检查误报成员缺失。
        install_names = {member["install_path"].split("/")[-1] for member in inference["members"]}
        assert "LICENSE-paddleocr.txt" not in install_names, (
            "许可成员名必须与发布物实际文件一致（models/paddleocr-onnx-models-LICENSE.txt），"
            "不得虚构 snapshot-ocr/LICENSE-paddleocr.txt"
        )
        assert "paddleocr-onnx-models-LICENSE.txt" in install_names, "许可成员必须在场"
    print("PASS OCR 资产清单结构完整")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
