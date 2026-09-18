"""pywin32 pywintypes 的最小类型桩（项目自维护）：仅覆盖 scripts/gui_smoke.py 用到的 API 面.

pywin32 未随包提供类型信息；本桩的类层级与官方 types-pywin32 一致（error/com_error 均直接继承 Exception）。
"""


class error(Exception):
    """Win32 API 调用失败的异常（pywin32 各扩展模块抛出的 error 均为此类或其别名）。"""


class com_error(Exception):
    """COM 调用失败的异常（pythoncom.com_error 与其为同一个类）。"""
