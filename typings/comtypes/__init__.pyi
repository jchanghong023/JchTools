# comtypes 的最小类型桩（项目自维护）：仅覆盖 gui_smoke.py 用到的异常类型。
#
# pywinauto 的 UIA 后端经 comtypes 访问 COM；窗口建立/销毁竞态下 COM 方法调用失败抛 COMError。
# （桩文件不放 docstring：类型桩只承载 API 面，语义说明以注释维护。）

class COMError(Exception): ...
