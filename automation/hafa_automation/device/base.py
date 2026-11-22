"""设备接口抽象。Android / iOS 实现必须保持同一套方法。"""
from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any


# Hafa 是 Flutter：content-desc 优先，text/resource-id 只保留为兼容选项。
STATUS_BAR_BOTTOM = 90
LOCATOR_PRIORITY = ("description", "description_contains", "point", "xpath", "text", "resource_id")


@dataclass(frozen=True)
class Locator:
    by: str
    value: Any

    def __post_init__(self):
        if self.by not in LOCATOR_PRIORITY:
            raise ValueError(f"不支持的定位方式: {self.by}")

    def to_dict(self) -> dict[str, Any]:
        return {"by": self.by, "value": self.value}

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "Locator":
        return cls(by=d["by"], value=d["value"])


@dataclass
class Element:
    index: int
    description: str
    text: str
    resource_id: str
    class_name: str
    clickable: bool
    bounds: tuple[int, int, int, int]  # left, top, right, bottom

    @property
    def center(self) -> tuple[int, int]:
        l, t, r, b = self.bounds
        return (l + r) // 2, (t + b) // 2

    @property
    def input_point(self) -> tuple[int, int]:
        """点击输入框用的位置：底边往上 40px。

        Flutter 的输入框语义节点常把上面的标题（如 Room Name）一起包进来，
        取中心会点到标题上、输入框没聚焦。矮节点（≤80px）退化为中心。
        """
        l, t, r, b = self.bounds
        return (l + r) // 2, b - min(40, (b - t) // 2)

    @property
    def in_status_bar(self) -> bool:
        """状态栏（时间、网速、电量等）里的节点：底边不超过 90px。

        页面标题的 top 可能只有 99，所以必须按 bottom 判断，不能按 top。
        """
        return self.bounds[3] <= STATUS_BAR_BOTTOM


class Device(ABC):
    @abstractmethod
    def launch_app(self, package: str, activity: str | None = None) -> None: ...

    @abstractmethod
    def stop_app(self, package: str) -> None: ...

    @abstractmethod
    def screenshot(self) -> bytes: ...

    @abstractmethod
    def hierarchy(self, retry: int | None = None) -> str: ...

    @abstractmethod
    def elements(self, retry: int | None = None) -> list[Element]: ...

    @abstractmethod
    def tap(self, x: int, y: int) -> None: ...

    @abstractmethod
    def tap_element(self, locators: list[Locator]) -> Locator: ...

    @abstractmethod
    def input_text(self, text: str, clear: bool = True) -> None: ...

    @abstractmethod
    def swipe(self, direction: str, scale: float = 0.6) -> None: ...

    @abstractmethod
    def back(self) -> None: ...

    def drag(self, x1: int, y1: int, x2: int, y2: int, duration: float = 0.8) -> None:
        """从 (x1,y1) 按住拖到 (x2,y2)。滑块、拖拽排序等用；swipe 只能滑动整个页面。"""
        raise NotImplementedError

    @abstractmethod
    def window_size(self) -> tuple[int, int]: ...

    # 便捷方法
    def tap_center(self, element: Element) -> None:
        x, y = element.center
        self.tap(x, y)

    def tap_input(self, element: Element) -> None:
        x, y = element.input_point
        self.tap(x, y)
