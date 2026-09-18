"""pywinauto 的最小类型桩（项目自维护）：仅覆盖 scripts/gui_smoke.py 用到的 API 面.

pywinauto 没有任何官方或 typeshed 类型信息，本包为其手写最小桩；
成员的真实实现以 pywinauto 0.6.9 源码为准（application.py / timings.py 等）。
"""

from . import controls as controls
from . import findbestmatch as findbestmatch
from . import findwindows as findwindows
from . import timings as timings
from .application import Application as Application
from .application import Desktop as Desktop
