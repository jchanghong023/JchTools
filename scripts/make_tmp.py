#!/usr/bin/env python3
"""JchTools 临时目录工厂：统一生成与清理本项目需要的所有 `.tmp/` 内容（自包含，不依赖仓库外目录）.

种类（后续新增临时数据需求时在此扩展）：
    testdata  手工测试数据集（嵌套包、冲突、垃圾文件、边界名称等），默认生成到
              <repo>/.tmp/testdata/，每次重建前先清空目标；`_测试说明.md` 逐条
              说明每个用例与预期行为。
    ocr-compare  从已校验的原模型和字体生成冻结 Python 版的独立测试 bundle，
                 只写入 .tmp/ocr-compare-bundle/。
    workspace  创建验证工作目录，不清空既有内容；供日志、修复前反证和隔离资产使用。

用法：
    python scripts/make_tmp.py testdata [--git] [--destination <专门测试目录>] [--force]
    python scripts/make_tmp.py ocr-compare --model-root <原模型目录> --font <固定字体文件> [--force]
    python scripts/make_tmp.py clean    # 清空整个 .tmp/ 释放磁盘；测试完成后执行，防止无限增长
    python scripts/make_tmp.py workspace [--destination <.tmp 内目录>]

clean 只删除仓库内 `.tmp/` 的内容，绝不触碰仓库其他位置与仓库外目录。

实现拆分：具体实现位于 scripts/make_tmp_impl/ 包（fsutil/corpus/guard）；本模块保留
argparse CLI、构建器注册表与 main 流程，并 re-export 测试引用的顶层名（seven_zip/
_clear_existing_target/robust_rmtree 等），保证 patch 目标语义（函数读定义处模块全局）不变。
"""

from __future__ import annotations

import argparse
import hashlib
import shutil
import stat
import sys
from pathlib import Path

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    if not (Path(__file__).resolve().parent / "make_tmp_impl" / "__init__.py").is_file():
        # 本文件被单独复制到合成 scripts/ 目录运行（可信基回归）时，向上定位最近的实现包。
        for _parent in Path(__file__).resolve().parents:
            if (_parent / "scripts" / "make_tmp_impl" / "__init__.py").is_file():
                sys.path.insert(0, str(_parent))
                break

from scripts.make_tmp_impl.corpus import (
    _GIT_COMMIT_IDENTITY,
    build_archive_conflicts,
    build_archives,
    build_broken,
    build_conflicts,
    build_containers,
    build_deep,
    build_duplicates,
    build_empty_and_chain,
    build_encrypted,
    build_formats,
    build_hidden,
    build_hostile,
    build_junk,
    build_large,
    build_multipart,
    build_nested,
    build_plain,
    build_readme,
    initialise_git,
)
from scripts.make_tmp_impl.fsutil import (
    _remove_reparse_point,
    _reparse_point_status,
    fail,
    force_remove_tree,
    measure_tree,
    robust_rmtree,
    run,
    seven_zip,
)
from scripts.make_tmp_impl.guard import guard_destination


def clean_tmp(repo_root: Path) -> int:
    """清空仓库内 .tmp/ 全部内容（一次性中间产物，测试完成后清理，防止磁盘无限增长）."""
    tmp = (repo_root / ".tmp").resolve()
    if tmp.name != ".tmp" or tmp.parent != repo_root:
        fail(f"安全检查失败：目标不是仓库内 .tmp/（{tmp}）")
    if not tmp.is_dir():
        print(f".tmp 不存在，无需清理：{tmp}")
        return 0
    count, total = measure_tree(tmp)
    failures: list[tuple[Path, str]] = []
    for entry in tmp.iterdir():
        try:
            reparse = _reparse_point_status(entry)
            if reparse is not None:
                _remove_reparse_point(entry, reparse)
            elif entry.is_dir() and not entry.is_symlink():
                robust_rmtree(entry)
            else:
                entry.chmod(stat.S_IWRITE)
                entry.unlink()
        except FileNotFoundError:
            pass
        except OSError as error:
            failures.append((entry, str(error)))
    if failures:
        for path, error in failures:
            print(f"删除失败：{path}：{error}")
        fail(f"共 {len(failures)} 个条目删除失败（见上方原因）；请处理后重跑 clean。")
    print(f"已清空 {tmp}：删除 {count} 个文件，释放约 {total / 1048576:.1f} MiB。")
    return 0


class _Arguments(argparse.Namespace):
    """显式声明各参数类型的命令行容器（argparse 会把解析结果 setattr 到该实例上）.

    类属性默认值与 argparse 的默认行为一一对应（位置参数必有值、
    --destination 默认 None、store_true 默认 False），因此不影响运行结果。
    """

    kind: str = ""
    destination: str | None = None
    git: bool = False
    force: bool = False
    model_root: str | None = None
    font: str | None = None


def _clear_existing_target(root: Path, *, force: bool) -> None:
    """目标已存在时：非目录直接拒绝；非空时统计并（按需确认）清空."""
    if not root.exists():
        return
    if _reparse_point_status(root) is not None:
        fail(f"拒绝清理 reparse point 目标：{root}")
    if not root.is_dir():
        fail(f"目标不是目录：{root}；请改用专门的测试目录。")
    entries = list(root.iterdir())
    if entries:
        count, total = measure_tree(root)
        print(f"目标目录非空：{root}")
        print(f"  将删除 {count} 个文件，合计 {total / 1048576:.1f} MiB（顶层 {len(entries)} 个条目）")
        if not force:
            if not sys.stdin.isatty():
                fail("目标目录非空；非交互环境请加 --force 确认清空。")
            try:
                answer = input("确认清空以上内容？输入 yes 继续：")
            except (EOFError, KeyboardInterrupt):
                fail("已取消：未清空目标目录。")
            if answer.strip().lower() != "yes":
                fail("已取消：未清空目标目录。")
    print(f"clearing {root} ...")
    force_remove_tree(root)


def build_ocr_compare_bundle(repo_root: Path, model_root: str | None, font_path: str | None, *, force: bool) -> int:
    """在 .tmp/ 下生成经模型、有序字典配置及字体摘要校验的冻结 Python 对照 bundle."""
    if not model_root or not font_path:
        fail("ocr-compare 需要 --model-root 和 --font")
    models = Path(model_root).resolve(strict=True)
    font = Path(font_path).resolve(strict=True)
    root = repo_root / ".tmp" / "ocr-compare-bundle"
    guard_destination(root)
    checks = (
        (
            models / "PP-OCRv6_small_det" / "inference.onnx",
            "3914f972d833af87d23bb2338bd09238f978a48f3c4dbb8e1a4ee26a93869940",
        ),
        (
            models / "PP-OCRv6_small_rec" / "inference.onnx",
            "3e3def686ac9a1676b59bc9749ad896263d8f68b53f352060774de359a2e23ed",
        ),
        (
            models / "PP-OCRv6_small_rec" / "inference.yml",
            "ab078671bb49f06228eadccd34f1bb501e157f7a047095ffb943ba81512c77d1",
        ),
        (font, "ec04cc376b34887cedbdf84074e2e226ed2761eeabdcb9173fc1dd7bfd153ef7"),
    )
    for path, expected in checks:
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != expected:
            fail(f"OCR 对照资产缺失或摘要不符：{path}")
    _clear_existing_target(root, force=force)
    for name in ("PP-OCRv6_small_det", "PP-OCRv6_small_rec"):
        _ = shutil.copytree(models / name, root / "models" / name)
    dest_font = root / "assets" / "fonts" / font.name
    dest_font.parent.mkdir(parents=True)
    _ = shutil.copy2(font, dest_font)
    for name in ("temp", "func_ret", "locks"):
        (root / "runtime" / "pdx-cache" / name).mkdir(parents=True)
    print(f"OCR 对照 bundle: {root}")
    return 0


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description="JchTools 临时目录工厂：生成与清理 .tmp/ 内容。")
    _ = parser.add_argument(
        "kind",
        choices=["testdata", "ocr-compare", "workspace", "clean"],
        help="testdata=生成测试数据；ocr-compare=生成旧 OCR 对照；workspace=创建隔离目录；clean=清空 .tmp/",
    )
    _ = parser.add_argument(
        "--destination",
        default=None,
        help="testdata/workspace：另指定 .tmp 内的目录；testdata 会先清空，workspace 保留既有内容",
    )
    _ = parser.add_argument("--git", action="store_true", help="testdata 专用：建立 git 基线并生成 恢复.ps1")
    _ = parser.add_argument("--model-root", help="ocr-compare 专用：含 det/rec 目录的固定原模型根目录")
    _ = parser.add_argument("--font", help="ocr-compare 专用：固定 NotoSansMonoCJKsc-Regular.otf")
    _ = parser.add_argument(
        "--force",
        action="store_true",
        help="testdata 专用：目标目录非空时免交互确认清空",
    )
    arguments = parser.parse_args(namespace=_Arguments())
    kind = arguments.kind
    force = arguments.force
    use_git = arguments.git
    destination = arguments.destination
    if kind == "clean":
        return clean_tmp(repo_root)
    if kind == "ocr-compare":
        return build_ocr_compare_bundle(repo_root, arguments.model_root, arguments.font, force=force)
    if kind == "workspace":
        root = Path(destination) if destination else repo_root / ".tmp" / "validation"
        guard_destination(root)
        root = root.expanduser().resolve()
        if not root.is_relative_to(repo_root / ".tmp"):
            fail("验证工作目录必须在仓库 .tmp/ 内")
        root.mkdir(parents=True, exist_ok=True)
        print(f"workspace: {root}")
        return 0
    root = Path(destination) if destination else repo_root / ".tmp" / "testdata"
    guard_destination(root)
    # guard 内部用 resolve() 检查，但构建全程用的是原始路径；相对路径在脚本切换
    # 工作目录后会指向不存在的位置（已清空目标却构建失败），这里统一转成绝对路径。
    root = root.expanduser().resolve()
    if not root.is_relative_to(repo_root / ".tmp"):
        fail("测试数据目录必须在仓库 .tmp/ 内")
    seven = seven_zip()

    _clear_existing_target(root, force=force)
    root.mkdir(parents=True)

    log: list[str] = []
    for builder in (
        build_plain,
        build_duplicates,
        build_conflicts,
        build_junk,
        build_empty_and_chain,
        build_formats,
        build_archives,
        build_archive_conflicts,
        build_nested,
        build_deep,
        build_broken,
        build_encrypted,
        build_multipart,
        build_large,
        build_hidden,
        build_hostile,
        build_containers,
    ):
        builder(root, seven, log)
        print(f"  built {builder.__name__}")
    if use_git:
        initialise_git(root, log)
    build_readme(root, log, git=use_git)
    # README 在 baseline 之后生成：必须再提交一次，否则 恢复.ps1 的 git clean -fd 会删掉说明。
    if use_git:
        # 与 baseline 提交同款内联身份：见实现包内 _GIT_COMMIT_IDENTITY 注释。
        run(["git", "-C", str(root), "add", "-A"])
        run(
            [
                "git",
                "-C",
                str(root),
                *_GIT_COMMIT_IDENTITY,
                "commit",
                "-m",
                "test corpus readme",
            ]
        )

    files = [p for p in root.rglob("*") if p.is_file() and ".git" not in p.parts]
    total = sum(p.stat().st_size for p in files)
    print(f"corpus: {root}")
    print(f"files: {len(files)}  size: {total / 1048576:.1f} MiB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
