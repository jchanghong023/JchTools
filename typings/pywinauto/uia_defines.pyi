# UIA 控件没有 InvokePattern 时的错误；复选框须回退到受控鼠标操作。
class NoPatternInterfaceError(Exception): ...

# IUIA 单例实际加载的 UIAutomationClient 常量，供原生能力查询使用。
class _UIAutomationConstants:
    UIA_IsValuePatternAvailablePropertyId: int

class IUIA:
    UIA_dll: _UIAutomationConstants
