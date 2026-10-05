# 只描述验收实际使用的 UIA 编辑控件与底层 COM 属性读取接口。
from pywinauto.base_wrapper import BaseWrapper

class _AutomationElement:
    def GetCurrentPropertyValue(self, property_id: int) -> object: ...  # noqa: N802 - COM 真实方法名。

class _ElementInfo:
    element: _AutomationElement

class EditWrapper(BaseWrapper):
    element_info: _ElementInfo

    def get_value(self) -> str: ...
