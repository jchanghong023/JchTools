"""Python / Rust / 质量命令的 Bazel 输入静态分析层.

从 gate_inputs 根模块拆出的分析闭包：Python import 与静态 Path 表达式、
unittest 发现与逐用例参数、质量工具配置范围、Rust dep-info 与集成测试
运行材料。共享原语（_UnknownInputsError/_local/_label/_make_words/
_table/_toml_table）仍由 gate_inputs 根模块提供，本模块不回写根模块；
根模块的 InputResolver 在仓库进程中调用这里的入口。Bazel 沙箱只复制
gate_inputs 单文件，适配器路径（cargo_unused_inputs）不经过本模块。
"""

from __future__ import annotations

import ast
import fnmatch
import re
import sys
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.gate_inputs import _label, _local, _make_words, _table, _toml_table, _UnknownInputsError

if TYPE_CHECKING:
    from scripts.gate_inputs import InputResolver
    from scripts.gate_runtime import Stage


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
