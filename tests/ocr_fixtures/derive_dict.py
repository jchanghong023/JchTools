"""从原 PP-OCRv6 small rec inference.yml 派生固定顺序字典.

需要 PyYAML；只向指定的 .tmp/ 路径写入，逐字节校验已发布的资产身份。
用法：python tests/ocr_fixtures/derive_dict.py <inference.yml> .tmp/ocr-assets/dict-check.txt
"""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path

import yaml

EXPECTED_SHA256 = "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d"
EXPECTED_CHARACTERS = 18708
EXPECTED_BYTES = 74947
REPO_ROOT = Path(__file__).resolve().parents[2]


def ensure_repo_tmp_output(path: Path) -> Path:
    """输出必须位于仓库根的真实 .tmp/ 之下（AGENTS.md §2），拒绝重解析点逃逸."""
    resolved = path.resolve()
    repo_root = REPO_ROOT.resolve()
    expected_tmp_root = repo_root / ".tmp"
    tmp_root = (REPO_ROOT / ".tmp").resolve()
    if tmp_root != expected_tmp_root or tmp_root not in resolved.parents:
        message = f"输出必须位于仓库 {expected_tmp_root} 之下：{resolved}"
        raise ValueError(message)
    return resolved


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    output = ensure_repo_tmp_output(args.output)
    source = yaml.safe_load(args.source.read_text(encoding="utf-8"))
    chars = source["PostProcess"]["character_dict"]
    if len(chars) != EXPECTED_CHARACTERS or not all(isinstance(char, str) and len(char) == 1 for char in chars):
        message = "字典条数或字符格式错误"
        raise ValueError(message)
    content = ("\n".join(chars) + "\n").encode("utf-8")
    if len(content) != EXPECTED_BYTES or hashlib.sha256(content).hexdigest() != EXPECTED_SHA256:
        message = "派生字典与发布资产摘要不符"
        raise ValueError(message)
    output.write_bytes(content)
    print(f"PASS 字典 {len(chars)} 字，SHA-256 {EXPECTED_SHA256}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
