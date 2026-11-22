"""生成脚本使用的 UI 接口：以 content-desc 定位，坐标和 XPath 兜底。"""
from __future__ import annotations

import re
import time

from .device.base import Element, Locator

LIST_TOP, LIST_BOTTOM = 163, 1487


class Ui:
    """Page Object 共用的设备操作和断言等待。"""

    def __init__(self, serial: str | None = None):
        # 延后加载设备配置，让收集脚本和导出契约不依赖设备环境变量。
        from . import ui

        self.serial = serial or ui._env("HAFA_SERIAL")
        self.pkg, self.act = ui.PKG, ui.ACT
        self.dev = ui.dev if self.serial == ui.SERIAL else ui.AndroidDevice(self.serial)
        self._ui = ui

    def elements(self, retry: int = 4):
        """读取非状态栏元素；每次读取最多请求三次控件树。"""
        return self._ui.elements(retry, device=self.dev)

    def _labels(self, elements: list[Element]) -> list[str]:
        return [self._ui.label(element).strip() for element in elements if element.description or element.text]

    def labels(self) -> list[str]:
        """返回页面文字，换行统一为「 / 」。没有描述时可读取原生文字。"""
        return self._labels(self.elements())

    def _find(self, text: str, exact: bool, candidates: list[Element], clickable: bool | None = None):
        return self._ui.find(text, exact, candidates=candidates, clickable=clickable, description_only=True)

    def find(self, text: str, *, exact: bool = True, clickable: bool | None = None):
        """按 content-desc 找元素，优先返回可点元素；没有匹配则返回 None。"""
        return self._find(text, exact, self.elements(), clickable)

    def has(self, text: str, *, exact: bool = False) -> bool:
        """检查页面文字，不等待。"""
        text = self._ui.normalize_label(text).strip()
        return any((label == text if exact else text in label) for label in self.labels() if label)

    def read(self, pattern: str) -> str | None:
        """返回第一条匹配正则的页面文字。"""
        regex = re.compile(pattern)
        return next((label for label in self.labels() if regex.search(label)), None)

    def wait_text(self, text: str, timeout: float = 10, *, exact: bool = False) -> None:
        """等到页面出现该文字，超时抛出断言错误。"""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.has(text, exact=exact):
                return
            time.sleep(0.5)
        raise AssertionError(f"等待超时：页面没有出现「{text}」。当前页面文字：{self.labels()[:15]}")

    def wait_gone(self, text: str, timeout: float = 10) -> None:
        """等到页面上的文字消失。"""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if not self.has(text):
                return
            time.sleep(0.5)
        raise AssertionError(f"等待超时：页面上的「{text}」没有消失")

    def wait_any(self, texts: list[str], timeout: float = 10) -> str:
        """等到任一候选文字出现，按参数顺序返回先匹配的候选。"""
        deadline = time.time() + timeout
        while time.time() < deadline:
            for text in texts:
                if self.has(text):
                    return text
            time.sleep(0.5)
        raise AssertionError(f"等待超时：{texts} 都没有出现。当前页面文字：{self.labels()[:15]}")

    def tap_xy(self, x: int, y: int, wait: float = 1.0) -> None:
        """点击没有描述的固定位置图标。"""
        self._ui.tap_xy(int(x), int(y), wait, device=self.dev)

    def tap(self, text: str, *, xy: tuple[int, int] | None = None, exact: bool = True,
            scroll: bool = False, wait: float = 1.0) -> None:
        """先按描述定位。原坐标还在命中范围内时点原坐标，否则点元素中心。

        找不到描述才用坐标；两者都没有时，text 为显式路径（以 / 或 .// 开头）才尝试 XPath。
        """
        target = self.scroll_to(text, exact=exact) if scroll else self.find(text, exact=exact)
        if target is not None:
            left, top, right, bottom = target.bounds
            if xy is None or not (left <= xy[0] <= right and top <= xy[1] <= bottom):
                xy = target.center
        if xy is not None:
            self.tap_xy(*xy, wait=wait)
            return
        if text.startswith(("/", ".//")):
            try:
                self.dev.tap_element([Locator("xpath", text)])
            except LookupError:
                pass
            else:
                self.wait(wait)
                return
        raise AssertionError(f"找不到「{text}」。当前页面文字：{self.labels()[:15]}")

    def scroll_to(self, text: str, *, exact: bool = True, max_swipes: int = 10):
        """向上滑动，直到描述匹配的元素完整可见、页面不动或达到上限。"""
        if max_swipes < 0:
            return None
        elements = self.elements()
        for swipes in range(max_swipes + 1):
            target = self._find(text, exact, elements)
            if target is not None:
                _, top, _, bottom = target.bounds
                if bottom - top >= 100:
                    visible = top >= LIST_TOP and bottom <= LIST_BOTTOM
                else:
                    visible = top >= 100 and bottom <= 1480
                if visible:
                    return target
            if swipes == max_swipes:
                break
            self.swipe()
            next_elements = self.elements()
            if self._labels(next_elements) == self._labels(elements):
                return None
            elements = next_elements
        return None

    def type_after(self, prev_label: str, text: str, *, xy: tuple[int, int] | None = None) -> None:
        """在控件树中指定描述后的第一个可点输入框输入；清空旧值并收起键盘。"""
        elements = self.elements()
        anchor = self._ui.normalize_label(prev_label).strip()
        index = next((index for index, element in enumerate(elements)
                      if anchor and self._ui.label(element, description_only=True).strip() == anchor), None)
        target = None
        if index is not None:
            target = next((element for element in elements[index + 1:] if element.clickable), None)
        if target is not None:
            self.dev.tap_input(target)
        elif xy is not None:
            self.tap_xy(*xy, wait=0)
        else:
            raise AssertionError(f"找不到「{prev_label}」后面的输入框。当前页面文字：{self._labels(elements)[:15]}")
        self.wait(0.6)
        self.dev.input_text(text)

    def drag(self, x1: int, y1: int, x2: int, y2: int, duration: float = 0.8, wait: float = 1.0) -> None:
        """按住拖动；滑块的起点和终点应远离屏幕边缘。"""
        self.dev.drag(int(x1), int(y1), int(x2), int(y2), duration)
        self.wait(wait)

    def swipe(self, direction: str = "up", wait: float = 0.8) -> None:
        """滑动整个页面，方向为 up / down / left / right。"""
        self.dev.swipe(direction)
        self.wait(wait)

    def back(self, wait: float = 1.0) -> None:
        """按系统返回键。"""
        self.dev.back()
        self.wait(wait)

    def restart_app(self, wait: float = 6) -> None:
        """强行停止后冷启动应用。"""
        self.dev.stop_app(self.pkg)
        self.wait(1.5)
        self.dev.launch_app(self.pkg, self.act)
        self.wait(wait)

    def wait(self, seconds: float) -> None:
        """固定等待；只用于本来就要等时间的步骤。"""
        time.sleep(seconds)

    def debug_overlay_up(self) -> bool:
        """检测语音调试面板的全屏 Close 遮罩。"""
        return any(self._ui.label(element).strip() == "Close" and element.bounds == (0, 0, 720, 1600)
                   for element in self.elements())

    def dismiss_debug_overlay(self) -> bool:
        """最多尝试关闭语音调试面板三次，返回是否点击过关闭按钮。"""
        closed = False
        for _ in range(3):
            if not self.debug_overlay_up():
                break
            self.tap_xy(668, 1005, wait=1.5)
            closed = True
        return closed

    def close_popups(self) -> None:
        """关闭语音调试、首页活动和签到弹窗，不执行签到。"""
        self.dismiss_debug_overlay()
        for _ in range(4):
            if self.has("Close activities"):
                self.tap_xy(360, 1265, wait=1.5)
            elif self.has("Close sign-in"):
                self.tap("Close sign-in", exact=True)
            else:
                break
