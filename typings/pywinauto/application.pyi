"""pywinauto.application 的最小类型桩：Application / Desktop / WindowSpecification 与进程查找错误."""

from .base_wrapper import BaseWrapper
from .win32structures import RECT


class ProcessNotFoundError(Exception):
    """Application.connect() 找不到目标进程时抛出。"""


class Application:
    """已连接/已启动目标进程的应用对象。"""

    def __init__(
        self, backend: str = "win32", datafilename: str | None = None, allow_magic_lookup: bool = True
    ) -> None:
        ...

    def connect(
        self, *, process: int, timeout: float | None = None, retry_interval: float | None = None
    ) -> Application:
        """连接到已运行进程（返回自身）；进程不存在时抛 ProcessNotFoundError。"""
        ...

    def window(self, **criteria: object) -> WindowSpecification:
        """child_window() 的弃用别名；gui_smoke 沿用旧写法，运行期仍会发 DeprecationWarning。"""
        ...


class Desktop:
    """桌面（全部顶层窗口）的窗口规格入口。"""

    def child_window(self, **criteria: object) -> WindowSpecification:
        ...

    def window(self, **criteria: object) -> WindowSpecification:
        ...


class WindowSpecification:
    """窗口/控件规格：惰性解析；运行期把未知属性转发给解析出的控件包装器.

    未在此声明的成员（如 click_input 的其他参数）真实存在，但 gui_smoke 未用到，故不列出。
    """

    # 运行期经 __getattribute__ 转发到包装器（wrapper.handle），声明为 int。
    handle: int

    def child_window(self, **criteria: object) -> WindowSpecification:
        """以当前规格为父级追加匹配条件，返回新的窗口规格。"""
        ...

    def click_input(self) -> None:
        """转发到包装器：真实鼠标点击。"""
        ...

    def descendants(self, **criteria: object) -> list[BaseWrapper]:
        """转发到包装器：按条件枚举全部后代控件。"""
        ...

    def exists(self, timeout: float | None = None, retry_interval: float | None = None) -> bool:
        """控件当前是否可解析存在（内部吞掉「找不到」类错误并返回 False）。"""
        ...

    def is_enabled(self) -> bool:
        """转发到包装器：控件（及其顶层窗口）是否可用。"""
        ...

    def is_visible(self) -> bool:
        """转发到包装器：控件（及其顶层窗口）是否可见。"""
        ...

    def rectangle(self) -> RECT:
        """转发到包装器：控件屏幕矩形。"""
        ...

    def set_focus(self) -> BaseWrapper:
        """转发到包装器：置前并聚焦（UIA 包装器返回自身）。"""
        ...

    def wait(self, wait_for: str, timeout: float | None = None, retry_interval: float | None = None) -> BaseWrapper:
        """等待控件进入 wait_for 描述的状态（如 "visible enabled"），超时抛 timings.TimeoutError。"""
        ...
