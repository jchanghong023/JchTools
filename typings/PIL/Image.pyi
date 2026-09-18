"""PIL.Image 的最小类型桩：make-icon.py 用到的 new/paste/alpha_composite/resize/save 与 LANCZOS."""

import os
from enum import IntEnum
from typing import Final


class Resampling(IntEnum):
    """重采样过滤器（取值与 Pillow 一致）。"""

    BICUBIC = 3
    LANCZOS = 1


LANCZOS: Final = Resampling.LANCZOS


class Image:
    """位图图像对象。"""

    def alpha_composite(self, im: Image, dest: tuple[int, int] = (0, 0), source: tuple[int, int] = (0, 0)) -> None:
        """把 im 按 alpha 通道合成到自身（就地修改，返回 None）。"""
        ...

    def paste(self, im: Image, box: tuple[int, int], mask: Image) -> None:
        """把 im 粘贴到 box 指定位置，以 mask 为透明蒙版（就地修改，返回 None）。"""
        ...

    def resize(self, size: tuple[int, int], resample: Resampling = Resampling.BICUBIC) -> Image:
        """缩放到 size；resample 指定重采样过滤器。"""
        ...

    def save(self, fp: str | os.PathLike[str], format: str | None = None, **params: object) -> None:
        """保存图像；format 缺省时按扩展名推断，其余关键字参数透传给具体格式插件。"""
        ...


def new(mode: str, size: tuple[int, int], color: float | tuple[float, ...] | str | None = 0) -> Image:
    """新建指定模式与尺寸的图像；color 为填充色（默认黑色）。"""
    ...
