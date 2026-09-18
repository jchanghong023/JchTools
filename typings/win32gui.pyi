"""pywin32 win32gui 的最小类型桩（项目自维护）：仅覆盖 scripts/gui_smoke.py 用到的窗口操作函数.

签名与官方 types-pywin32 一致；参数为位置形参（真实 pywin32 同样只接受位置传参）。
失败时抛 pywintypes.error。
"""

def ShowWindow(hWnd: int, cmdShow: int, /) -> int:
    """设置窗口的显示状态（cmdShow 取 win32con.SW_* 常量）；返回非零表示成功。"""
    ...


def SetWindowPos(hWnd: int, hWndInsertAfter: int, X: int, Y: int, cx: int, cy: int, uFlags: int, /) -> None:
    """设置窗口位置/大小/z 序（hWndInsertAfter 取 win32con.HWND_* 常量，配合 SWP_* 标志）。"""
    ...
