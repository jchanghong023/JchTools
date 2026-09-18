"""PIL.ImageDraw 的最小类型桩：make-icon.py 用到的绘图方法."""

from collections.abc import Sequence
from typing import TypeAlias

from .Image import Image

Ink: TypeAlias = float | tuple[float, ...] | str | None


class ImageDraw:
    """2D 绘图接口。"""

    def line(self, xy: Sequence[tuple[float, float]], fill: Ink = None, width: int = 1) -> None:
        """在给定点之间画线段。"""
        ...

    def polygon(self, xy: Sequence[tuple[float, float]], fill: Ink = None, outline: Ink = None) -> None:
        """按给定点序画多边形。"""
        ...

    def rounded_rectangle(self, xy: Sequence[float], radius: float = 0, fill: Ink = None) -> None:
        """画圆角矩形；xy 为（左，上，右，下），radius 为圆角半径。"""
        ...


def Draw(im: Image, mode: str | None = None) -> ImageDraw:
    """为图像创建绘图接口（就地写入 im）。"""
    ...
