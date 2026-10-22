"""转 Markdown 验收共享路径常量（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

from pathlib import Path


__all__ = [
    "ASSET_MANIFEST",
    "FIXTURES_DEFAULT",
    "GUI_SMOKE",
    "ROOT",
    "SCRATCH_ROOT",
    "TMP_ROOT",
]


ROOT = Path(__file__).resolve().parent.parent.parent


TMP_ROOT = ROOT / ".tmp"


FIXTURES_DEFAULT = ROOT / "tests" / "markdown_fixtures"


SCRATCH_ROOT = TMP_ROOT / "markdown-acceptance"


GUI_SMOKE = ROOT / "scripts" / "gui_smoke.py"


ASSET_MANIFEST = ROOT / "resources" / "markdown-assets.json"
