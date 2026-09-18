"""comtypes 的最小类型桩（项目自维护）：仅覆盖 gui_smoke.py 用到的异常类型.

pywinauto 的 UIA 后端经 comtypes 访问 COM；窗口建立/销毁竞态下 COM 方法调用失败抛 COMError。
"""


class COMError(Exception):
    """COM 方法调用失败（携带失败的 HRESULT）。"""
