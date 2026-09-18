"""pywin32 win32con 的最小类型桩（项目自维护）：仅声明 scripts/gui_smoke.py 用到的常量.

取值与官方 types-pywin32（即真实 Win32 常量）一致。
"""

from typing import Final

SWP_NOSIZE: Final = 1  # 保持当前大小（忽略 cx/cy）
SWP_NOMOVE: Final = 2  # 保持当前位置（忽略 x/y）
SWP_SHOWWINDOW: Final = 64  # 显示窗口
SW_RESTORE: Final = 9  # 激活并显示窗口；若最小化/最大化则还原到原尺寸与位置
HWND_TOP: Final = 0  # 置于 z 序最顶（不改变激活状态，配合 SWP_ 标志使用）
