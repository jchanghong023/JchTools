# pywinauto.base_wrapper 的最小类型桩：控件包装器基类（gui_smoke 用到的转发目标）。
#
# 成员的真实实现分布于 base_wrapper.py / uiawrapper.py；gui_smoke 只用到下列 API 面。
# （桩文件不放 docstring：类型桩只承载 API 面，语义说明以注释维护。）

from .win32structures import RECT

class InvalidElement(RuntimeError): ...

# 控件包装器基类。
class BaseWrapper:
    handle: int

    # 真实鼠标点击（移动光标并按下/抬起鼠标键）。
    def click_input(self) -> None: ...

    # 按条件枚举全部后代控件。
    def descendants(self, **criteria: object) -> list[BaseWrapper]: ...

    # 控件（及其顶层窗口）是否可用。
    def is_enabled(self) -> bool: ...

    # 控件（及其顶层窗口）是否可见。
    def is_visible(self) -> bool: ...

    # 控件的屏幕矩形。
    def rectangle(self) -> RECT: ...

    # 写入编辑框文本（真实定义在编辑类控件包装器；gui_smoke 经 descendants 取得后调用）。
    def set_edit_text(self, text: str) -> None: ...

    # 置前并聚焦（UIA 包装器返回自身）。
    def set_focus(self) -> BaseWrapper: ...

    # 控件可见文本（可能为 None）。
    def window_text(self) -> str | None: ...
