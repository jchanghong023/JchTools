"""为门发现逻辑 unittest ID，或用原报告运行器精确执行一个 ID.

--list -- <unittest 参数> 只加载；--case <ID> -- <unittest 参数> 保留
所有匹配实例。subTest 和测试方法内部循环仍属于父方法，不单独枚举。
"""

from __future__ import annotations

import argparse
import json
import sys
import traceback
import unittest
from pathlib import Path
from typing import TYPE_CHECKING, override

if TYPE_CHECKING:
    from collections.abc import Callable, Iterator

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from scripts import unittest_report


def _cases(test: unittest.TestSuite | unittest.TestCase) -> Iterator[unittest.TestCase]:
    if isinstance(test, unittest.TestSuite):
        for child in test:
            yield from _cases(child)
    else:
        yield test


class _Options(argparse.Namespace):
    list_cases: bool = False
    case: str | None = None
    loaded: bool = False


def _arguments(args: list[str], options: _Options) -> list[str]:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    _ = mode.add_argument("--list", action="store_true", dest="list_cases")
    _ = mode.add_argument("--case")
    try:
        separator = args.index("--")
    except ValueError:
        parser.error("请在 unittest 参数之前提供 -- 分隔符")
    _ = parser.parse_args(args[:separator], namespace=options)
    return args[separator + 1 :]


def _program(options: _Options, notify: Callable[[str, BaseException | None], None]) -> type[unittest.TestProgram]:
    class LoadingErrors(list[type[BaseException]]):
        # typeshed 将 loader.errors 标为异常类型列表；CPython 实际追加格式化字符串。
        @override
        def append(self, error: type[BaseException]) -> None:
            notify(str(error), None)
            super().append(error)

    loader = unittest.TestLoader()
    loader.errors = LoadingErrors()

    class GateProgram(unittest.TestProgram):
        @override
        def __init__(
            self,
            *,
            module: None,
            argv: list[str],
            testRunner: type[unittest.TextTestRunner],
        ) -> None:
            self.test: unittest.TestSuite | unittest.TestCase = unittest.TestSuite()
            super().__init__(module=module, argv=argv, testRunner=testRunner, testLoader=loader)

        @override
        def runTests(self) -> None:
            if loader.errors:
                message = "unittest 加载失败：\n" + "\n".join(map(str, loader.errors))
                raise RuntimeError(message)
            cases = list(_cases(self.test))
            if options.list_cases:
                # 清单按 ID 去重；精确执行仍保留该 ID 的全部原始实例。
                ids = list(dict.fromkeys(test.id() for test in cases))
                print("JCHTOOLS_TEST_CASES=" + json.dumps(ids, ensure_ascii=True), flush=True)
                options.loaded = True
                raise SystemExit(0)
            selected = [test for test in cases if test.id() == options.case]
            if not selected:
                message = f"未找到 unittest 用例：{options.case}"
                raise ValueError(message)
            self.test = unittest.TestSuite(selected)
            options.loaded = True
            super().runTests()

    return GateProgram


def main(arguments: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if arguments is None else arguments)
    options = _Options()
    notified = False

    def notify_failure(detail: str, error: BaseException | None = None) -> None:
        nonlocal notified
        if not notified:
            notified = True
            unittest_report.notify_gate_failure(detail, error)

    try:
        test_arguments = _arguments(args, options)
        code = unittest_report.main(
            test_arguments,
            test_program=_program(options, notify_failure),
            command=[sys.executable, "-m", "scripts.gate_unittest", *args],
        )
    except SystemExit as error:
        code = error.code if isinstance(error.code, int) else 1
        if code:
            notify_failure("门的 unittest 参数无效", error)
        return code
    except (ImportError, RuntimeError, ValueError, TypeError, OSError, KeyboardInterrupt) as error:
        notify_failure("门的 unittest 加载或选择失败", error)
        traceback.print_exc()
        return 130 if isinstance(error, KeyboardInterrupt) else 1
    else:
        if not options.loaded:
            notify_failure("unittest 加载未完成")
            return code or 1
        return code


if __name__ == "__main__":
    sys.exit(main())
