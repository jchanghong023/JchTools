"""声明原生 Bazel 输入，不维护第二套结果或 mtime 缓存.

已知质量命令绑定原生命令范围；集成测试绑定编译产物、dep-info 与运行材料。
Cargo 绑定源码/manifest/config/资源发现范围及真实 include；build 程序先保留
完整输入，成功执行后才以当前 dep-info/Cargo 声明生成原生 unused_inputs_list。
未知 build/读取/配置不剔除输入；不把陈旧构建声明当成执行前的完整证明。
Python 入口绑定静态导入和可证明的常量 Path 读取；目录扫描、动态加载、
源码审查、未知 argv 或外部路径保守回退。解析缓存只存在于一次 dispatch，
Cargo 恢复或下一批动作生成前必须 refresh，不保存任何成功结果。
"""

from __future__ import annotations

import ast
import fnmatch
import json
import os
import re
import shutil
import sys
import tomllib
from pathlib import Path, PureWindowsPath
from typing import TYPE_CHECKING, cast

if TYPE_CHECKING:
    from scripts.gate_runtime import Stage


class _UnknownInputsError(Exception):
    """当前输入无法可靠细化，须保留原有保守闭包."""


def _local(root: Path, value: str | Path) -> Path:
    text = str(value)
    windows = PureWindowsPath(text)
    # 不把其它卷或 UNC 路径误当相对路径；由 Windows Path 处理本机路径。
    if windows.drive and not Path(text).is_absolute():
        raise _UnknownInputsError
    if windows.root and not windows.drive and text.startswith("\\"):
        raise _UnknownInputsError
    path = Path(text.replace("\\", "/"))
    if not path.is_absolute():
        path = root / path
    path = path.resolve(strict=True)
    if not path.is_relative_to(root):
        raise _UnknownInputsError
    return path


def _label(root: Path, path: Path) -> str:
    relative = path.relative_to(root).as_posix()
    # Bazel label 不能表示所有合法的文件系统名称。
    if not relative or any(character in relative for character in ":\n\r\\"):
        raise _UnknownInputsError
    if not path.is_file():
        raise _UnknownInputsError
    return "//:" + relative


def _make_words(value: str) -> list[str]:
    """解码 Make 转义，同时保留 Windows 原生路径分隔符."""
    words: list[str] = []
    word: list[str] = []
    index = 0
    while index < len(value):
        character = value[index]
        if character == "\\" and index + 1 < len(value):
            following = value[index + 1]
            if following in " \\#:$\t":
                word.append(following)
                index += 2
                continue
        if character.isspace():
            if word:
                words.append("".join(word))
                word = []
        else:
            word.append(character)
        index += 1
    if word:
        words.append("".join(word))
    return words


def _env_value(value: str) -> str:
    """解码 Rustc dep-info 转义，不套用 Python 字符串转义规则."""
    result: list[str] = []
    index = 0
    escapes = {"\\": "\\", "n": "\n", "r": "\r"}
    while index < len(value):
        character = value[index]
        if character == "\\":
            index += 1
            if index >= len(value) or value[index] not in escapes:
                raise _UnknownInputsError
            character = escapes[value[index]]
        result.append(character)
        index += 1
    return "".join(result)


def _artifact(root: Path, path: Path) -> set[str]:
    labels = {_label(root, path)}
    # DLL 也是运行期输入；其它 EXE 只能由实际 CARGO_BIN_EXE 依赖引入。
    directories = [path.parent]
    if path.parent.name == "deps":
        directories.append(path.parent.parent)
    for directory in directories:
        for dll in directory.glob("*.dll"):
            labels.add(_label(root, _local(root, dll)))
    return labels


def _environment(root: Path, stage: Stage) -> set[str]:
    labels: set[str] = set()
    for key, value in (stage.env or {}).items():
        if key in {"PYTHONIOENCODING", "PYTHONUTF8", "RUST_BACKTRACE"}:
            continue
        # 显式夹具和程序覆盖仅能在可声明为仓库内文件时收紧依赖。
        if key.startswith("JCHTOOLS_") and key.endswith(("EXE", "7ZIP", "INPUT")):
            path = _local(root, value)
            labels.update(_artifact(root, path))
        else:
            raise _UnknownInputsError
    return labels


def _dep_info(root: Path, executable: Path, text: str) -> tuple[set[Path], set[Path]]:
    dependencies: set[Path] = set()
    programs: set[Path] = set()
    for line in text.replace("\\\r\n", "").replace("\\\n", "").splitlines():
        if line.startswith("# env-dep:CARGO_BIN_EXE_"):
            key, separator, value = line.removeprefix("# env-dep:").partition("=")
            if not separator or not key.removeprefix("CARGO_BIN_EXE_"):
                raise _UnknownInputsError
            programs.add(_local(root, _env_value(value)))
        elif line and not line.startswith("#"):
            # 盘符后的冒号紧跟路径分隔符，不是 Make 规则的空白分隔符。
            separator = re.search(r":(?:\s|$)", line)
            if separator is None:
                raise _UnknownInputsError
            targets = [_local(root, word) for word in _make_words(line[: separator.start()])]
            if executable in targets:
                dependencies.update(_local(root, word) for word in _make_words(line[separator.end() :]))
    if not dependencies:
        raise _UnknownInputsError
    return dependencies, programs


def _integration_sources(resolver: InputResolver, dependencies: set[Path]) -> set[str]:
    # 不按测试名猜依赖。源码审查、生产模块和不透明运行脚本仍使用完整闭包。
    scripts: set[Path] = set()
    for source in dependencies:
        if not source.relative_to(resolver.root).as_posix().startswith("tests/"):
            raise _UnknownInputsError
        if source.suffix != ".rs":
            continue
        body = resolver.text(source)
        if re.search(r"(?:crates[/\\]|optional[/\\]|target[/\\])", body):
            raise _UnknownInputsError
        references = [match[1] for match in re.finditer(r'"(scripts/[A-Za-z0-9_./-]+\.py)"', body)]
        # 拼接脚本名、内嵌解释器程序及模块入口无法仅由 Rust dep-info 证明。
        if re.search(r'"scripts"|scripts\\|(?:python|python3)"\s*\)\s*\.args?\(\s*(?:\[\s*)?"-', body):
            raise _UnknownInputsError
        if any(match[0] not in references for match in re.finditer(r"scripts/[^\"\s]+", body)):
            raise _UnknownInputsError
        if re.search(r'"python(?:3|\.exe)?"', body) and not references:
            raise _UnknownInputsError
        scripts.update(_local(resolver.root, value) for value in references)
    return resolver.python_closure(list(scripts), resolver.root)


def _rust_inputs(resolver: InputResolver, stage: Stage) -> list[str] | None:
    root = resolver.root
    executable = _local(root, stage.argv[0])
    relative = executable.relative_to(root).as_posix()
    if not re.fullmatch(r"(?:optional/[^/]+/)?target/debug/deps/[^/]+-[0-9a-f]+\.exe", relative):
        return None
    depfile = _local(root, executable.with_suffix(".d"))
    dependencies, programs = resolver.dep_info(executable, depfile)
    labels = _integration_sources(resolver, dependencies)
    # 非脚本运行材料保持保守；只有实际执行/读取的脚本进入 Python 闭包。
    labels.update({"//:gate_test_materials", _label(root, depfile)})
    labels.update(_artifact(root, executable))
    for program in programs:
        labels.update(_artifact(root, program))
    labels.update(_environment(root, stage))
    return sorted(labels)


def _module(root: Path, name: str, search: Path) -> list[Path]:
    if not name or any(not part.isidentifier() for part in name.split(".")):
        raise _UnknownInputsError
    relative = Path(*name.split("."))
    for base in dict.fromkeys((search, root)):
        candidate = base / relative
        source = candidate.with_suffix(".py")
        package = candidate / "__init__.py"
        if source.is_file() or package.is_file():
            result = [_local(root, source if source.is_file() else package)]
            parent = candidate.parent
            while parent != base and parent.is_relative_to(base):
                initializer = parent / "__init__.py"
                if initializer.is_file():
                    result.append(_local(root, initializer))
                parent = parent.parent
            return result
    return []


def _import_names(root: Path, source: Path, node: ast.AST) -> list[tuple[str, bool]]:
    if isinstance(node, ast.Import):
        return [(alias.name, True) for alias in node.names]
    if not isinstance(node, ast.ImportFrom):
        return []
    prefix = node.module or ""
    if node.level:
        package = source.parent.relative_to(root).parts
        if node.level > len(package):
            raise _UnknownInputsError
        prefix = ".".join((*package[: len(package) - node.level + 1], prefix)).strip(".")
    if not prefix:
        return []
    return [(prefix, True), *((prefix + "." + alias.name, False) for alias in node.names if alias.name != "*")]


def _check_python_node(node: ast.AST) -> None:
    if isinstance(node, ast.Name) and node.id == "__import__":
        raise _UnknownInputsError
    if isinstance(node, ast.Call):
        function = node.func
        if (isinstance(function, ast.Name) and function.id == "__import__") or (
            isinstance(function, ast.Attribute)
            and function.attr in {"import_module", "spec_from_file_location", "exec_module"}
        ):
            raise _UnknownInputsError


def _path_call(node: ast.Call, source: Path, names: dict[str, Path]) -> Path:
    if not node.keywords:
        if isinstance(node.func, ast.Name) and node.func.id == "Path" and len(node.args) == 1:
            return _static_path(node.args[0], source, names)
        if isinstance(node.func, ast.Attribute) and node.func.attr in {"resolve", "absolute"} and not node.args:
            return _static_path(node.func.value, source, names)
    raise _UnknownInputsError


def _path_parent(node: ast.Subscript, source: Path, names: dict[str, Path]) -> Path:
    if isinstance(node.value, ast.Attribute) and node.value.attr == "parents" and isinstance(node.slice, ast.Constant):
        index = node.slice.value
        if isinstance(index, int) and index >= 0:
            return _static_path(node.value.value, source, names).parents[index]
    raise _UnknownInputsError


def _path_name(node: ast.Name, source: Path, names: dict[str, Path]) -> Path:
    if node.id == "__file__":
        return source
    if node.id in names:
        return names[node.id]
    raise _UnknownInputsError


def _static_path(node: ast.AST, source: Path, names: dict[str, Path]) -> Path:
    """只计算不依赖运行值的 Path 表达式；其余表达式一律未知."""
    if isinstance(node, ast.Name):
        return _path_name(node, source, names)
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return Path(node.value)
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Div):
        if isinstance(node.right, ast.Constant) and isinstance(node.right.value, str):
            return _static_path(node.left, source, names) / node.right.value
    elif isinstance(node, ast.Attribute) and node.attr == "parent":
        return _static_path(node.value, source, names).parent
    elif isinstance(node, ast.Subscript):
        return _path_parent(node, source, names)
    elif isinstance(node, ast.Call):
        return _path_call(node, source, names)
    raise _UnknownInputsError


def _path_stores(nodes: tuple[ast.AST, ...]) -> dict[str, int]:
    stores: dict[str, int] = {}
    for node in nodes:
        if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Store):
            stores[node.id] = stores.get(node.id, 0) + 1
        elif isinstance(node, ast.arg):
            stores[node.arg] = stores.get(node.arg, 0) + 2
    return stores


def _check_path_binding(nodes: tuple[ast.AST, ...], stores: dict[str, int]) -> None:
    if stores.get("Path") or stores.get("__file__"):
        raise _UnknownInputsError
    if not any(
        isinstance(node, ast.ImportFrom)
        and node.module == "pathlib"
        and any(alias.name == "Path" and alias.asname in {None, "Path"} for alias in node.names)
        for node in nodes
    ):
        raise _UnknownInputsError
    if any(
        isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)) and node.name == "Path"
        for node in nodes
    ):
        raise _UnknownInputsError


def _path_assignment(statement: ast.stmt) -> tuple[ast.expr, ast.expr] | None:
    if isinstance(statement, ast.Assign) and len(statement.targets) == 1:
        return statement.targets[0], statement.value
    if isinstance(statement, ast.AnnAssign) and statement.value is not None:
        return statement.target, statement.value
    return None


def _static_paths(nodes: tuple[ast.AST, ...], source: Path) -> dict[str, Path]:
    names: dict[str, Path] = {}
    stores = _path_stores(nodes)
    _check_path_binding(nodes, stores)
    module = nodes[0]
    if not isinstance(module, ast.Module):
        raise _UnknownInputsError
    for statement in module.body:
        assignment = _path_assignment(statement)
        if assignment is None:
            continue
        target, value = assignment
        if isinstance(target, ast.Name) and stores.get(target.id) == 1:
            try:
                names[target.id] = _static_path(value, source, names)
            except _UnknownInputsError:
                continue
    return names


_RUNTIME_INPUT_ATTRIBUTES = frozenset(
    {
        "read_text",
        "read_bytes",
        "open",
        "glob",
        "rglob",
        "iterdir",
        "walk",
        "listdir",
        "scandir",
        "exists",
        "is_file",
        "is_dir",
        "stat",
        "access",
        "readlink",
        "run",
        "Popen",
        "check_call",
        "check_output",
        "call",
        "system",
        "popen",
        "copy",
        "copyfile",
        "copy2",
        "copytree",
        "move",
        "load",
        "getsource",
        "getsourcefile",
        "getfile",
    }
)


def _runtime_input(resolver: InputResolver, source: Path, node: ast.Call) -> str | None:
    function = node.func
    if isinstance(function, ast.Attribute) and function.attr in {
        "read_text",
        "read_bytes",
        "exists",
        "is_file",
        "is_dir",
        "stat",
    }:
        path = _static_path(function.value, source, resolver.paths(source))
        return _label(resolver.root, _local(resolver.root, path))
    # 文件/目录扫描及子进程可能读取任意源码，不能仅以 import 闭包替代。
    if (isinstance(function, ast.Name) and function.id in {"open", "eval", "exec", "getattr", "globals", "locals"}) or (
        isinstance(function, ast.Attribute) and function.attr in _RUNTIME_INPUT_ATTRIBUTES
    ):
        raise _UnknownInputsError
    return None


def _python_imports(resolver: InputResolver, source: Path, search: Path, node: ast.AST) -> list[Path]:
    root = resolver.root
    imports: list[Path] = []
    # 直接导入或改名不会改变文件读取/扫描的语义，不能只识别属性调用。
    if isinstance(node, ast.ImportFrom) and any(alias.name in _RUNTIME_INPUT_ATTRIBUTES for alias in node.names):
        raise _UnknownInputsError
    for name, required in _import_names(root, source, node):
        if (name.startswith("importlib") and not name.startswith("importlib.metadata")) or name in {
            "scripts.static_check",
            "scripts.requirement_coverage",
        }:
            raise _UnknownInputsError
        resolved = resolver.module(name, search)
        if resolved:
            imports.extend(resolved)
        elif required and (root / name.split(".")[0]).exists() and not (root / Path(*name.split("."))).is_dir():
            raise _UnknownInputsError
    return imports


def _python_closure(resolver: InputResolver, sources: list[Path], search: Path, *, runtime: bool) -> set[str]:
    root = resolver.root
    labels: set[str] = set()
    pending = list(sources)
    seen: set[Path] = set()
    while pending:
        source = pending.pop()
        if source in seen:
            continue
        seen.add(source)
        labels.add(_label(root, source))
        for node in resolver.nodes(source):
            _check_python_node(node)
            if runtime and isinstance(node, ast.Call):
                label = _runtime_input(resolver, source, node)
                if label is not None:
                    labels.add(label)
                    continue
            # 报告基础设施可通过固定 argv 调用脚本，不一定使用 Python import。
            if (
                not runtime
                and isinstance(node, ast.Constant)
                and isinstance(node.value, str)
                and re.fullmatch(r"scripts/[A-Za-z0-9_./-]+\.py", node.value)
            ):
                pending.append(_local(root, node.value))
            pending.extend(_python_imports(resolver, source, search, node))
    return labels


_UNITTEST_FLAGS = frozenset({"-f", "--failfast", "-v", "--verbose", "-q", "--quiet", "-b", "--buffer"})


def _unittest_arguments(argv: list[str]) -> list[str] | None:
    match argv:
        case [_, "-m", "scripts.unittest_report", *arguments] if arguments:
            return arguments
        case [_, "-m", "scripts.gate_unittest", *arguments] if arguments:
            separator = arguments.index("--")
            match arguments[:separator]:
                case ["--list"]:
                    return arguments[separator + 1 :]
                case ["--case", identifier] if identifier:
                    return arguments[separator + 1 :]
                case _:
                    raise _UnknownInputsError
        case _:
            return None


def _discovered_sources(root: Path, arguments: list[str]) -> tuple[Path, list[Path]]:
    directory, pattern = ".", "test*.py"
    index = 1
    while index < len(arguments):
        option = arguments[index]
        if option in _UNITTEST_FLAGS:
            index += 1
            continue
        # 自定义顶级导入目录及未知参数保持保守回退。
        if option not in {"-s", "--start-directory", "-p", "--pattern"}:
            raise _UnknownInputsError
        value = arguments[index + 1]
        if option in {"-s", "--start-directory"}:
            directory = value
        else:
            pattern = value
        index += 2
    search = _local(root, directory)
    if not search.is_dir() or any(character in pattern for character in "/\\"):
        raise _UnknownInputsError
    sources = [_local(root, path) for path in search.rglob("*.py") if fnmatch.fnmatchcase(path.name, pattern)]
    return search, sources


def _selected_sources(root: Path, arguments: list[str]) -> list[Path]:
    selected: list[Path] = []
    for argument in arguments:
        if argument in _UNITTEST_FLAGS:
            continue
        resolved = _module(root, argument, root)
        if not resolved:
            raise _UnknownInputsError
        selected.extend(resolved)
    if not selected:
        raise _UnknownInputsError
    return selected


def _table(value: object) -> dict[str, object]:
    if not isinstance(value, dict):
        raise _UnknownInputsError
    return cast("dict[str, object]", value)


def _toml_table(resolver: InputResolver, source: Path) -> dict[str, object]:
    return _table(cast("object", tomllib.loads(resolver.text(source))))


def _pyright_config(root: Path, tools: dict[str, object]) -> None:
    if (root / "pyrightconfig.json").exists():
        raise _UnknownInputsError
    settings = _table(tools.get("basedpyright", {}))
    if settings.get("include") != ["scripts", "typings"] or settings.get("stubPath") != "typings":
        raise _UnknownInputsError
    if set(settings) & {
        "extends",
        "extraPaths",
        "executionEnvironments",
        "typeshedPath",
        "venvPath",
        "venv",
        "pythonPath",
    }:
        raise _UnknownInputsError


def _ruff_config(resolver: InputResolver, tools: dict[str, object]) -> None:
    root = resolver.root
    if "extend" in _table(tools.get("ruff", {})):
        raise _UnknownInputsError
    for directory in (root, root / "scripts", root / "typings"):
        paths = (
            [directory / "ruff.toml", directory / ".ruff.toml"]
            if directory == root
            else [*directory.rglob("ruff.toml"), *directory.rglob(".ruff.toml")]
        )
        for path in paths:
            if path.is_file() and "extend" in _toml_table(resolver, path):
                raise _UnknownInputsError
    # 子目录 pyproject.toml 可以另声明 extend，同样不能遗漏其外部配置。
    for directory in (root / "scripts", root / "typings"):
        for path in directory.rglob("pyproject.toml"):
            nested = _toml_table(resolver, path)
            if "extend" in _table(_table(nested.get("tool", {})).get("ruff", {})):
                raise _UnknownInputsError


def _python_config(resolver: InputResolver, tool: str) -> None:
    """配置可以扩大输入域；无法证明仍在声明范围内时不采用窄 filegroup."""
    project = resolver.root / "pyproject.toml"
    configuration = _toml_table(resolver, project) if project.is_file() else {}
    tools = _table(configuration.get("tool", {}))
    if tool == "basedpyright":
        _pyright_config(resolver.root, tools)
    elif tool == "ruff":
        _ruff_config(resolver, tools)


def _quality_file_inputs(root: Path, argv: list[str]) -> list[str] | None:
    match argv:
        case [_, "-m", "vulture", *arguments] if arguments[-2:] == ["--min-confidence", "100"]:
            files = arguments[:-2]
        case [_, "-m", "bandit", *arguments] if arguments[-3:] == ["-s", "B404,B603", "-q"]:
            if (root / ".bandit").exists():
                raise _UnknownInputsError
            files = arguments[:-3]
        case _:
            return None
    if not files:
        raise _UnknownInputsError
    # argv 中显式文件之外仍绑定发现范围，以使新增生产脚本自动失效。
    labels = {"//:gate_python_quality_sources", "//:gate_python_quality_config"}
    for value in files:
        source = _local(root, value)
        if source.suffix != ".py":
            raise _UnknownInputsError
        labels.add(_label(root, source))
    return sorted(labels)


def _quality_inputs(resolver: InputResolver, stage: Stage) -> list[str] | None:
    """匹配实际 argv；未知选项不借用相似命令的范围."""
    if stage.argv[0] != sys.executable:
        return None
    match stage.argv:
        case [_, "-m", "scripts.quality_scope", "syntax"]:
            return ["//:gate_python_syntax_sources", "//:scripts/quality_scope.py"]
        case [_, "-m", "scripts.quality_scope", "rustfmt"]:
            return ["//:gate_rust_format_sources", "//:scripts/quality_scope.py"]
        case [_, "-m", "ruff", "format", "--check", "scripts", "typings"] | [
            _,
            "-m",
            "ruff",
            "check",
            "scripts",
            "typings",
        ]:
            _python_config(resolver, "ruff")
            return ["//:gate_python_quality_sources", "//:gate_python_quality_config"]
        case [_, "-m", "basedpyright"]:
            _python_config(resolver, "basedpyright")
            return ["//:gate_python_quality_sources", "//:gate_python_quality_config"]
        case _:
            return _quality_file_inputs(resolver.root, stage.argv)


def _python_inputs(resolver: InputResolver, stage: Stage) -> list[str] | None:
    root = resolver.root
    if stage.argv[0] != sys.executable:
        return None
    arguments = _unittest_arguments(stage.argv)
    if arguments is None:
        match stage.argv:
            case [_, script] if script.endswith(".py"):
                source = _local(root, script)
            case _:
                return None
        labels = resolver.python_closure([source], source.parent)
        labels.update(_environment(root, stage))
        return sorted(labels)
    sources = [_local(root, "scripts/unittest_report.py")]
    if stage.argv[2] == "scripts.gate_unittest":
        sources.append(_local(root, "scripts/gate_unittest.py"))
    if arguments[:1] == ["discover"]:
        search, selected = _discovered_sources(root, arguments)
    else:
        search, selected = root, _selected_sources(root, arguments)
    # 报告基础设施只写报告/历史源码身份；其读取不改变被测用例的语义。
    # 若测试本身导入并断言这些功能，selected 的严格闭包仍会保守回退。
    labels = resolver.python_closure(sources, root, runtime=False)
    labels.update(resolver.python_closure(selected, search))
    # 每批重新做模块解析/发现：新增模块、包初始化文件及测试会改变 label 集，
    # 不需要将不相关 Python 文件的内容也作为每个 unittest 的输入。
    labels.update(_environment(root, stage))
    return sorted(labels)


# The repository's declared build-material contract covers these build programs:
# root icon/manifest, assets JSON, UI compiler and embedded 7-Zip resources.
# Unknown build programs retain the full closure. Refinement for these programs
# happens only AFTER execution, using current Cargo declarations and dep-info.
_CARGO_BUILD_CONTRACTS = frozenset(
    {
        "build.rs",
        "crates/assets/build.rs",
        "crates/ui/build.rs",
        "crates/file-core/build.rs",
        "optional/snap-ocr-worker/build.rs",
    }
)
_CARGO_PRUNED = frozenset(
    {".git", ".tmp", ".venv", "venv", "target", "__pycache__", ".ruff_cache", ".pytest_cache", "vendor"}
)
_RUST_STRING = r'r(?P<hashes>\#*)"(.*?)"(?P=hashes)|"(?:\\.|[^"\\])*"'
_RUST_TOKEN = re.compile(
    r"//[^\n]*|/\*|'(?:\\.|[^'\\\n])'|" + _RUST_STRING + r"|[A-Za-z_][A-Za-z_0-9]*|[^\s]", re.DOTALL
)


def _rust_tokens(text: str) -> list[str]:
    """Keep string literals atomic, ignoring even nested Rust block comments."""
    tokens: list[str] = []
    position = 0
    while position < len(text):
        match = _RUST_TOKEN.search(text, position)
        if match is None:
            break
        lexeme = match[0]
        position = match.end()
        if lexeme == "/*":
            depth = 1
            while depth:
                opening = text.find("/*", position)
                closing = text.find("*/", position)
                if closing < 0:
                    raise _UnknownInputsError
                if 0 <= opening < closing:
                    depth += 1
                    position = opening + 2
                else:
                    depth -= 1
                    position = closing + 2
        elif not lexeme.startswith("//"):
            tokens.append(lexeme)
    return tokens


def _rust_literal(lexeme: str) -> str:
    if lexeme.startswith("r"):
        match = re.fullmatch(_RUST_STRING, lexeme, re.DOTALL)
        if match is not None:
            return match[2]
    if lexeme.startswith('"') and lexeme.endswith('"'):
        # Normal filesystem literals need no Rust escape decoding. Unsupported
        # escapes fall back rather than accidentally naming a different file.
        value = lexeme[1:-1]
        if "\\" not in value:
            return value
    raise _UnknownInputsError


def _include_environment(arguments: list[str], manifest: Path, *, generated: bool) -> Path | None:
    variable = _rust_literal(arguments[3])
    suffix = arguments[6:]
    if suffix[-1:] == [","]:
        suffix = suffix[:-1]
    if len(suffix) != 1:
        raise _UnknownInputsError
    value = _rust_literal(suffix[0])
    if variable == "OUT_DIR" and generated:
        return None
    if variable == "CARGO_MANIFEST_DIR":
        return manifest / value.lstrip("/\\")
    raise _UnknownInputsError


def _include_literals(arguments: list[str]) -> str:
    values: list[str] = []
    for index, lexeme in enumerate(arguments):
        if index % 2:
            if lexeme != ",":
                raise _UnknownInputsError
        else:
            values.append(_rust_literal(lexeme))
    return "".join(values)


def _include_path(tokens: list[str], source: Path, manifest: Path, *, generated: bool) -> Path | None:
    if tokens[-1:] == [","]:
        tokens = tokens[:-1]
    if len(tokens) == 1:
        return source.parent / _rust_literal(tokens[0])
    if tokens[:3] != ["concat", "!", "("] or tokens[-1:] != [")"]:
        raise _UnknownInputsError
    arguments = tokens[3:-1]
    prefix = arguments[:6]
    if prefix[:3] == ["env", "!", "("] and prefix[4:] == [")", ","]:
        return _include_environment(arguments, manifest, generated=generated)
    return source.parent / _include_literals(arguments)


def _cargo_files(root: Path) -> list[Path]:
    paths: list[Path] = []
    for directory, names, files in os.walk(root):
        names[:] = [name for name in names if name not in _CARGO_PRUNED and not name.startswith("bazel-")]
        paths.extend(Path(directory) / name for name in files)
    return paths


def _cargo_grouped(root: Path, path: Path) -> bool:
    relative = path.relative_to(root)
    if relative.parts[0] == "vendor" or path.suffix == ".rs" or path.name in {"Cargo.toml", "Cargo.lock"}:
        return True
    if path.parent.name == ".cargo" and path.name in {"config", "config.toml"}:
        return True
    if path.name in {"rust-toolchain", "rust-toolchain.toml", "BUILD.bazel"}:
        return True
    return relative.parts[0] in {"src", "crates", "optional", "tests", "ui", "resources"} and path.suffix not in {
        ".md",
        ".py",
        ".pyi",
        ".pyc",
    }


def _cargo_configuration(value: object) -> None:
    """Reject opaque tool/wrapper/path overrides, including nested Cargo tables."""
    if isinstance(value, dict):
        mapping = cast("dict[str, object]", value)
        for key, item in mapping.items():
            if key in {
                "rustc",
                "rustc-wrapper",
                "rustc-workspace-wrapper",
                "runner",
                "paths",
                "include",
                "target-dir",
                "build-dir",
            }:
                raise _UnknownInputsError
            # rust-lld is supplied by the selected Rust toolchain; both its
            # bundled binary and any PATH override are bound by tools-compile.
            if key == "linker" and item != "rust-lld":
                raise _UnknownInputsError
            if key in {"directory", "local-registry"} and item != "vendor":
                raise _UnknownInputsError
            _cargo_configuration(item)
    elif isinstance(value, list):
        for item in cast("list[object]", value):
            _cargo_configuration(item)


def _cargo_artifact_depfiles(root: Path, filenames: object) -> set[Path]:
    if not isinstance(filenames, list):
        raise _UnknownInputsError
    depfiles: set[Path] = set()
    for filename in cast("list[object]", filenames):
        if not isinstance(filename, str):
            raise _UnknownInputsError
        artifact = Path(filename)
        if artifact.suffix not in {".rlib", ".rmeta", ".exe", ".dll", ".so", ".dylib", ""}:
            continue
        candidate = artifact.with_suffix(".d")
        candidates = [candidate]
        if candidate.name.startswith("lib"):
            candidates.append(candidate.with_name(candidate.name[3:]))
        if artifact.stem == "build-script-build":
            # Cargo renames the executable; rustc retains its crate name/hash.
            candidates.extend(artifact.parent.glob("build_script_build*.d"))
        depfiles.update(_local(root, path) for path in candidates if path.is_file())
    if not depfiles:
        raise _UnknownInputsError
    return depfiles


def _cargo_observed_files(root: Path, log: Path) -> tuple[list[Path], list[Path]]:
    """Select metadata from this invocation, never stale target-directory scans."""
    depfiles: set[Path] = set()
    outputs: set[Path] = set()
    with log.open(encoding="utf-8", errors="replace") as source:
        for line in source:
            try:
                value = cast("object", json.loads(line))
            except ValueError:
                continue
            if not isinstance(value, dict):
                continue
            event = cast("dict[str, object]", value)
            if event.get("reason") == "build-script-executed":
                directory = event.get("out_dir")
                if not isinstance(directory, str):
                    raise _UnknownInputsError
                outputs.add(_local(root, Path(directory).parent / "output"))
            elif event.get("reason") == "compiler-artifact":
                depfiles.update(_cargo_artifact_depfiles(root, event.get("filenames")))
    if not depfiles:
        raise _UnknownInputsError
    return sorted(depfiles), sorted(outputs)


def _cargo_home_configuration(resolver: InputResolver, environment: dict[str, str]) -> None:
    cargo_home_value = environment.get("CARGO_HOME")
    if cargo_home_value is None:
        home = environment.get("USERPROFILE") or environment.get("HOME")
        if home is None:
            raise _UnknownInputsError
        cargo_home = Path(home) / ".cargo"
    else:
        cargo_home = Path(cargo_home_value)
    if not cargo_home.is_absolute():
        cargo_home = resolver.root / cargo_home
    # Cargo merges configuration from the invocation directory through every
    # ancestor, then CARGO_HOME. The adapter fingerprints these same external
    # files, so inherited configuration is not silently treated as absent.
    directories = [resolver.root, *resolver.root.parents]
    configurations = [directory / ".cargo" / name for directory in directories for name in ("config", "config.toml")]
    configurations.extend(cargo_home / name for name in ("config", "config.toml"))
    for configuration in dict.fromkeys(configurations):
        if configuration.is_file():
            _cargo_configuration(tomllib.loads(resolver.text(configuration)))


def _cargo_manifest_paths(root: Path, manifest: Path, value: object) -> set[str]:
    """Validate manifest paths with an explicit, non-captured package directory."""
    labels: set[str] = set()
    if isinstance(value, dict):
        for key, item in cast("dict[str, object]", value).items():
            if key == "path" and isinstance(item, str):
                selected = _local(root, manifest / item)
                if selected.is_file() and not _cargo_grouped(root, selected):
                    labels.add(_label(root, selected))
            else:
                labels.update(_cargo_manifest_paths(root, manifest, item))
    elif isinstance(value, list):
        for item in cast("list[object]", value):
            labels.update(_cargo_manifest_paths(root, manifest, item))
    return labels


def _cargo_manifest_build(root: Path, manifest: Path, package: dict[str, object], *, observed: bool) -> Path | None:
    build = package.get("build", "build.rs")
    if build is False:
        return None
    if not isinstance(build, str):
        raise _UnknownInputsError
    candidate = manifest / build
    if not candidate.exists():
        return None
    candidate = _local(root, candidate)
    if not observed or candidate.relative_to(root).as_posix() not in _CARGO_BUILD_CONTRACTS:
        raise _UnknownInputsError
    return candidate


def _cargo_manifests(
    resolver: InputResolver, manifests: set[Path], labels: set[str], *, observed: bool
) -> tuple[set[Path], dict[str, set[Path]], set[str]]:
    builds: set[Path] = set()
    package_roots: dict[str, set[Path]] = {}
    build_packages: set[str] = set()
    for manifest in manifests:
        data = _toml_table(resolver, manifest / "Cargo.toml")
        package = _table(data.get("package", {}))
        name = package.get("name")
        if isinstance(name, str):
            package_roots.setdefault(name, set()).add(manifest)
        build = _cargo_manifest_build(resolver.root, manifest, package, observed=observed)
        if build is not None:
            builds.add(build)
            if isinstance(name, str):
                build_packages.add(name)
        labels.update(_cargo_manifest_paths(resolver.root, manifest, data))
    return builds, package_roots, build_packages


def _rust_include_arguments(tokens: list[str], index: int) -> list[str]:
    if tokens[index + 2 : index + 3] != ["("]:
        raise _UnknownInputsError
    end, depth = index + 3, 1
    while end < len(tokens) and depth:
        depth += (tokens[end] == "(") - (tokens[end] == ")")
        end += 1
    if depth:
        raise _UnknownInputsError
    return tokens[index + 3 : end - 1]


def _cargo_source_includes(
    resolver: InputResolver, source: Path, manifest: Path, *, generated: bool
) -> list[tuple[Path, bool]]:
    includes: list[tuple[Path, bool]] = []
    tokens = _rust_tokens(resolver.text(source))
    for index, lexeme in enumerate(tokens):
        if lexeme not in {"include", "include_str", "include_bytes"} or tokens[index + 1 : index + 2] != ["!"]:
            continue
        arguments = _rust_include_arguments(tokens, index)
        included = _include_path(arguments, source, manifest, generated=generated)
        if included is not None:
            includes.append((_local(resolver.root, included), lexeme == "include"))
    return includes


def _cargo_literal_inputs(
    resolver: InputResolver, pending: list[Path], manifests: set[Path], *, generated: bool
) -> set[str]:
    labels: set[str] = set()
    visited: set[Path] = set()
    while pending:
        source = _local(resolver.root, pending.pop())
        if source in visited:
            continue
        visited.add(source)
        manifest = next((parent for parent in source.parents if parent in manifests), None)
        if manifest is None:
            raise _UnknownInputsError
        for included, recursive in _cargo_source_includes(resolver, source, manifest, generated=generated):
            if not _cargo_grouped(resolver.root, included):
                labels.add(_label(resolver.root, included))
            if recursive:
                pending.append(included)
    return labels


def _cargo_depfile_target(root: Path, depfile: Path, value: str) -> bool:
    target = Path(value)
    absolute = target if target.is_absolute() else root / target
    candidate = absolute.with_suffix(".d").resolve()
    return candidate == depfile or (
        candidate.name.startswith("lib") and candidate.with_name(candidate.name[3:]) == depfile
    )


def _cargo_depfile_words(resolver: InputResolver, depfiles: list[Path]) -> set[str]:
    dependencies: set[str] = set()
    for depfile in depfiles:
        file_dependencies: set[str] = set()
        for line in resolver.text(depfile).replace("\\\r\n", "").replace("\\\n", "").splitlines():
            if not line or line.startswith("#"):
                continue
            separator = re.search(r":(?:\s|$)", line)
            if separator is None:
                raise _UnknownInputsError
            words = _make_words(line[separator.end() :])
            if not words:
                # rustc also emits phony rules for individual source files.
                continue
            if not any(
                _cargo_depfile_target(resolver.root, depfile, word) for word in _make_words(line[: separator.start()])
            ):
                raise _UnknownInputsError
            file_dependencies.update(words)
        # A fresh Cargo artifact event does not prove an empty/truncated text
        # dep-info complete. Require a real associated rule in EVERY selected
        # file, not merely one nonempty dependency among all artifacts.
        if not file_dependencies:
            raise _UnknownInputsError
        dependencies.update(file_dependencies)
    return dependencies


def _cargo_depfile_inputs(resolver: InputResolver, depfiles: list[Path], target_roots: list[Path]) -> set[str]:
    labels: set[str] = set()
    for word in _cargo_depfile_words(resolver, depfiles):
        candidate = Path(word)
        candidate = (resolver.root / candidate).resolve() if not candidate.is_absolute() else candidate.resolve()
        if any(candidate.is_relative_to(directory) for directory in target_roots):
            continue
        dependency = _local(resolver.root, word)
        if not _cargo_grouped(resolver.root, dependency):
            labels.add(_label(resolver.root, dependency))
    return labels


def _cargo_declaration_matches(root: Path, value: str, declaration_roots: set[Path]) -> list[Path]:
    path = Path(value)
    if path.is_absolute():
        return [path]
    matches = [manifest / path for manifest in declaration_roots if (manifest / path).exists()]
    if not matches and not any(_cargo_grouped(root, manifest / path) for manifest in declaration_roots):
        raise _UnknownInputsError
    return matches


def _cargo_declared_inputs(root: Path, declarations: list[str], declaration_roots: set[Path]) -> set[str]:
    labels: set[str] = set()
    for value in declarations:
        for match in _cargo_declaration_matches(root, value, declaration_roots):
            selected = _local(root, match)
            files = selected.rglob("*") if selected.is_dir() else [selected]
            for file in files:
                if file.is_file() and not _cargo_grouped(root, file):
                    labels.add(_label(root, _local(root, file)))
    return labels


def _cargo_build_inputs(
    resolver: InputResolver, outputs: list[Path], package_roots: dict[str, set[Path]], build_packages: set[str]
) -> set[str]:
    labels: set[str] = set()
    for output in outputs:
        package_name = output.parent.name.rpartition("-")[0]
        declaration_roots = package_roots.get(package_name, set()) | set(
            (resolver.root / "vendor").glob(package_name + "-*")
        )
        declarations = [
            match[1]
            for line in resolver.text(output).splitlines()
            if (match := re.match(r"cargo::?rerun-if-changed=(.*)", line)) is not None
        ]
        if package_name in build_packages and not declarations:
            # Cargo's default for an undeclared build is the whole package.
            raise _UnknownInputsError
        labels.update(_cargo_declared_inputs(resolver.root, declarations, declaration_roots))
    return labels


def _cargo_discovery(resolver: InputResolver) -> tuple[list[Path], set[Path]]:
    paths = _cargo_files(resolver.root)
    manifests = {path.parent for path in paths if path.name == "Cargo.toml"}
    if resolver.root not in manifests:
        raise _UnknownInputsError
    for path in paths:
        if path.parent.name == ".cargo" and path.name in {"config", "config.toml"}:
            _cargo_configuration(tomllib.loads(resolver.text(path)))
    return paths, manifests


def _cargo_source_inputs(
    resolver: InputResolver, *, environment: dict[str, str], observed: bool = False, log: Path | None = None
) -> list[str]:
    root = resolver.root
    # Isolated workspaces without our discovery filegroup retain full inputs.
    if 'name = "gate_cargo_sources"' not in resolver.text(_local(root, "BUILD.bazel")):
        raise _UnknownInputsError
    _cargo_home_configuration(resolver, environment)
    paths, manifests = _cargo_discovery(resolver)
    labels = {"//:gate_cargo_sources"}
    if (root / "vendor").is_dir():
        labels.add("//:gate_vendor_sources")
    builds, package_roots, build_packages = _cargo_manifests(resolver, manifests, labels, observed=observed)
    # Collect new/edited literal includes before compilation as well.
    if not observed:
        pending = [path for path in paths if path.suffix == ".rs" and path not in builds]
        labels.update(_cargo_literal_inputs(resolver, pending, manifests, generated=bool(builds)))
    # Generated target files are outputs, never source inputs (nor the .d itself).
    target_roots = [root / "target", *(root / "optional").glob("*/target")]
    if observed:
        if log is None:
            raise _UnknownInputsError
        depfiles, outputs = _cargo_observed_files(root, log)
        labels.update(_cargo_build_inputs(resolver, outputs, package_roots, build_packages))
    else:
        depfiles = [depfile for target in target_roots for depfile in (target / "debug/deps").glob("*.d")]
    labels.update(_cargo_depfile_inputs(resolver, depfiles, target_roots))
    return sorted(labels)


def _cargo_argument_inputs(root: Path, arguments: list[str]) -> None:
    flags = {
        "--all-targets",
        "--all-features",
        "--no-default-features",
        "--workspace",
        "--lib",
        "--bins",
        "--tests",
        "--no-run",
        "--locked",
        "--offline",
        "--frozen",
        "--message-format=json",
    }
    values = {"--manifest-path", "--features", "-F", "--package", "-p", "--bin", "--test", "--exclude"}
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument in flags:
            index += 1
            continue
        option, separator, value = argument.partition("=")
        if option not in values:
            raise _UnknownInputsError
        if not separator:
            index += 1
            value = arguments[index]
        if not value or value.startswith("-"):
            raise _UnknownInputsError
        if option == "--manifest-path":
            _ = _local(root, value)
        index += 1


def _cargo_environment_inputs(environment: dict[str, str]) -> None:
    overrides = {
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_BUILD_TARGET",
    }
    for key, value in environment.items():
        name = key.upper()
        if (name in overrides and value) or (name.startswith("JCHTOOLS_") and not name.startswith("JCHTOOLS_GATE_")):
            raise _UnknownInputsError


def _cargo_invocation_inputs(root: Path, argv: list[str], environment: dict[str, str]) -> None:
    _cargo_environment_inputs(environment)
    executable = Path(argv[0])
    if executable.stem.lower() != "cargo" or executable.suffix.lower() not in {"", ".exe"}:
        raise _UnknownInputsError
    if executable.parent != Path():
        expected = shutil.which("cargo", path=environment.get("PATH", ""))
        if expected is None or executable.resolve() != Path(expected).resolve():
            raise _UnknownInputsError
    if argv[1] not in {"build", "check", "clippy", "test"}:
        raise _UnknownInputsError
    arguments = argv[2:]
    if argv[1] == "clippy" and arguments[-3:] == ["--", "-D", "warnings"]:
        arguments = arguments[:-3]
    if argv[1] == "test" and "--no-run" not in arguments:
        raise _UnknownInputsError
    _cargo_argument_inputs(root, arguments)


def cargo_unused_inputs(
    root: Path, inputs: list[str], argv: list[str], environment: dict[str, str], log: Path
) -> list[str]:
    """Refine only successful Cargo actions via native unused_inputs_list.

    The initial action declares the full repository. Only execution producing
    current dep-info/build declarations can remove unused sources. Unknown
    metadata retains everything; no success ledger or mtime validity is used.
    """
    try:
        resolver = InputResolver(root)
        _cargo_invocation_inputs(resolver.root, argv, environment)
        selected = _cargo_source_inputs(resolver, observed=True, environment=environment, log=log)
        needed = {label.removeprefix("//:") for label in selected}
        needed.add("scripts/gate_inputs.py")
        sources = {path.relative_to(resolver.root).as_posix() for path in _cargo_files(resolver.root)}
        unused: list[str] = []
        for value in inputs:
            relative = Path(value).as_posix()
            if (
                relative in sources
                and relative not in needed
                and not _cargo_grouped(resolver.root, resolver.root / relative)
            ):
                unused.append(value)
    except (OSError, ValueError, IndexError, SyntaxError, _UnknownInputsError):
        return []
    else:
        return unused


def _cargo_inputs(resolver: InputResolver, stage: Stage) -> list[str] | None:
    environment = dict(os.environ) | (stage.env or {})
    _cargo_invocation_inputs(resolver.root, stage.argv, environment)
    return resolver.cargo_inputs(environment)


class InputResolver:
    """一次 dispatch 的解析共享；不是成功缓存或基于 mtime 的有效性判定.

    调用者在每批 Cargo 产物恢复后、下一批输入解析前调用 refresh。缓存存放解析
    树/文本而非文件指纹；实际动作有效性始终由 Bazel 文件内容指纹判定。
    """

    def __init__(self, root: Path) -> None:
        self.root: Path = root.resolve(strict=True)
        self._texts: dict[Path, str] = {}
        self._nodes: dict[Path, tuple[ast.AST, ...]] = {}
        self._paths: dict[Path, dict[str, Path]] = {}
        self._modules: dict[tuple[str, Path], list[Path]] = {}
        self._deps: dict[tuple[Path, Path], tuple[set[Path], set[Path]]] = {}
        self._closures: dict[tuple[tuple[Path, ...], Path, bool], set[str] | None] = {}
        self._rust_labels: dict[tuple[str, tuple[tuple[str, str], ...]], list[str] | None] = {}
        self._python_labels: dict[tuple[str, tuple[str, ...], tuple[tuple[str, str], ...]], list[str] | None] = {}
        self._cargo_resolved: bool = False
        self._cargo_labels: list[str] | None = None

    def refresh(self) -> None:
        """丢弃上一批观测；包括失败前已解析的部分输入."""
        self._texts.clear()
        self._nodes.clear()
        self._paths.clear()
        self._modules.clear()
        self._deps.clear()
        self._closures.clear()
        self._rust_labels.clear()
        self._python_labels.clear()
        self._cargo_resolved = False
        self._cargo_labels = None

    def text(self, source: Path) -> str:
        if source not in self._texts:
            self._texts[source] = source.read_text(encoding="utf-8-sig")
        return self._texts[source]

    def nodes(self, source: Path) -> tuple[ast.AST, ...]:
        if source not in self._nodes:
            self._nodes[source] = tuple(ast.walk(ast.parse(self.text(source), filename=str(source))))
        return self._nodes[source]

    def paths(self, source: Path) -> dict[str, Path]:
        if source not in self._paths:
            self._paths[source] = _static_paths(self.nodes(source), source)
        return self._paths[source]

    def module(self, name: str, search: Path) -> list[Path]:
        key = name, search
        if key not in self._modules:
            self._modules[key] = _module(self.root, name, search)
        return self._modules[key]

    def dep_info(self, executable: Path, depfile: Path) -> tuple[set[Path], set[Path]]:
        key = executable, depfile
        if key not in self._deps:
            self._deps[key] = _dep_info(self.root, executable, self.text(depfile))
        return self._deps[key]

    def python_closure(self, sources: list[Path], search: Path, *, runtime: bool = True) -> set[str]:
        key = tuple(sorted(set(sources))), search, runtime
        if key not in self._closures:
            try:
                self._closures[key] = _python_closure(self, sources, search, runtime=runtime)
            except (OSError, ValueError, IndexError, SyntaxError, _UnknownInputsError):
                self._closures[key] = None
        labels = self._closures[key]
        if labels is None:
            raise _UnknownInputsError
        return set(labels)

    def cargo_inputs(self, environment: dict[str, str]) -> list[str] | None:
        """共享当前批次已解析的 Cargo 输入；未知结果仍保守回退."""
        if not self._cargo_resolved:
            self._cargo_resolved = True
            self._cargo_labels = _cargo_source_inputs(self, environment=environment)
        return self._cargo_labels

    def unittest_inputs(self, stage: Stage) -> list[str] | None:
        arguments = _unittest_arguments(stage.argv)
        if arguments is None:
            return _python_inputs(self, stage)
        # gate_unittest 的逐用例 ID 不是发现范围；同一原始 argv 只扫描一次。
        key = stage.argv[2], tuple(arguments), tuple(sorted((stage.env or {}).items()))
        if key not in self._python_labels:
            try:
                self._python_labels[key] = _python_inputs(self, stage)
            except (OSError, ValueError, IndexError, SyntaxError, _UnknownInputsError):
                self._python_labels[key] = None
        return self._python_labels[key]

    def rust_inputs(self, stage: Stage) -> list[str] | None:
        # 同一 libtest 的发现与逐用例只改变过滤 argv，共享产物/运行材料闭包。
        key = stage.argv[0], tuple(sorted((stage.env or {}).items()))
        if key not in self._rust_labels:
            try:
                self._rust_labels[key] = _rust_inputs(self, stage)
            except (OSError, ValueError, IndexError, SyntaxError, _UnknownInputsError):
                self._rust_labels[key] = None
        return self._rust_labels[key]

    def stage_inputs(self, stage: Stage) -> list[str] | None:
        """返回真实 argv 的原生 label；无法可靠识别时保守回退."""
        if not stage.argv:
            return None
        try:
            if stage.kind in {"compile", "compiler-observed"}:
                return _cargo_inputs(self, stage)
            if stage.kind != "check":
                return None
            quality = _quality_inputs(self, stage)
            if quality is not None:
                return sorted(set(quality) | _environment(self.root, stage))
            python = self.unittest_inputs(stage)
            return python if python is not None else self.rust_inputs(stage)
        except (OSError, ValueError, IndexError, SyntaxError, _UnknownInputsError):
            return None


def stage_inputs(root: Path, stage: Stage) -> list[str] | None:
    """兼容单次解析 API；批量调用应共享 InputResolver 并遵守 refresh 边界."""
    try:
        return InputResolver(root).stage_inputs(stage)
    except (OSError, ValueError):
        return None
