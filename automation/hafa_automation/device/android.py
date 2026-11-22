"""Android 设备实现（uiautomator2）。

针对 Hafa（Flutter）的适配：
- 控件树 text 为空、resource-id 基本没有，可读信息在 content-desc；
- tap_element 先按 content-desc 精确匹配，再模糊匹配，最后退回坐标；
- dump 偶尔返回空树，自动重试。
"""
from __future__ import annotations

import re
import time
import xml.etree.ElementTree as ET

import uiautomator2 as u2

from .base import Device, Element, Locator

BOUNDS_RE = re.compile(r"\[(\d+),(\d+)\]\[(\d+),(\d+)\]")
MAX_DUMP_ATTEMPTS = 3


def _parse_bounds(value: str) -> tuple[int, int, int, int]:
    m = BOUNDS_RE.match(value or "")
    if not m:
        return (0, 0, 0, 0)
    return tuple(int(x) for x in m.groups())  # type: ignore[return-value]


# 会走 HTTP 到手机上 uiautomator2 服务的方法：App 被强杀、服务重启时会偶发断连（RemoteDisconnected 等），
# 这些调用遇到连接类错误时自动重连重试，而不是让整条用例失败。
_RESILIENT = {"click", "swipe", "screenshot", "send_keys", "press", "shell",
              "app_start", "app_stop", "window_size"}


class _Resilient:
    """u2 设备的薄包装：_RESILIENT 里的方法遇到连接类错误时重连并重试 3 次，其余属性原样透传。"""

    def __init__(self, owner: "AndroidDevice"):
        self._owner = owner

    def __getattr__(self, name: str):
        attr = getattr(self._owner._raw(), name)
        if name not in _RESILIENT or not callable(attr):
            return attr

        def call(*args, **kwargs):
            return self._call(lambda client: getattr(client, name)(*args, **kwargs))

        return call

    def _call(self, action):
        for attempt in range(3):
            try:
                return action(self._owner._raw())
            except Exception as error:
                kind = type(error).__name__
                if not (isinstance(error, OSError) or any(
                        name in kind for name in ("Disconnect", "Connect", "RPC", "HTTP", "Timeout", "Protocol"))):
                    raise
                self._owner._reset()
                if attempt == 2:
                    raise
                time.sleep(2 * (attempt + 1))

    def __call__(self, *a, **k):
        return self._owner._raw()(*a, **k)               # d(description=...) 选择器


class AndroidDevice(Device):
    def __init__(self, serial: str, retries: int = MAX_DUMP_ATTEMPTS):
        self.serial = serial
        self.retries = retries
        self._d = None
        self._proxy = _Resilient(self)

    def _raw(self):
        if self._d is None:
            self._d = u2.connect(self.serial)
        return self._d

    def _reset(self) -> None:
        self._d = None                                   # 下次调用重新 u2.connect

    @property
    def d(self):
        return self._proxy

    # ---------- 应用 ----------
    def launch_app(self, package: str, activity: str | None = None) -> None:
        if activity:
            self.d.app_start(package, activity)
        else:
            self.d.app_start(package)

    def stop_app(self, package: str) -> None:
        self.d.app_stop(package)

    # ---------- 观察 ----------
    def screenshot(self) -> bytes:
        """返回 PNG 字节。"""
        import io

        img = self.d.screenshot()
        buf = io.BytesIO()
        img.save(buf, format="PNG")
        return buf.getvalue()

    def hierarchy(self, retry: int | None = None) -> str:
        attempts = min(self.retries, MAX_DUMP_ATTEMPTS)
        if retry is not None:
            attempts = min(attempts, retry)
        last = ""
        for attempt in range(attempts):
            # u2 的 dump_hierarchy 自带重试；调用单次 RPC，避免叠加空树重试。
            last = self._proxy._call(
                lambda client: client.jsonrpc.dumpWindowHierarchy(False, client.settings["max_depth"])
            ) or ""
            if "<node" in last:
                return last
            if attempt + 1 < attempts:
                time.sleep(0.6 * (attempt + 1))
        return last

    def elements(self, retry: int | None = None) -> list[Element]:
        xml = self.hierarchy(retry)
        if not xml.strip():
            return []
        root = ET.fromstring(xml)
        out: list[Element] = []
        for i, node in enumerate(root.iter("node")):
            desc = node.get("content-desc") or ""
            text = node.get("text") or ""
            rid = node.get("resource-id") or ""
            # 只保留有信息或可点击的节点
            if not (desc or text or rid or node.get("clickable") == "true"):
                continue
            out.append(
                Element(
                    index=i,
                    description=desc,
                    text=text,
                    resource_id=rid,
                    class_name=node.get("class") or "",
                    clickable=node.get("clickable") == "true",
                    bounds=_parse_bounds(node.get("bounds", "")),
                )
            )
        return out

    def window_size(self) -> tuple[int, int]:
        return tuple(self.d.window_size())  # type: ignore[return-value]

    # ---------- 操作 ----------
    def tap(self, x: int, y: int) -> None:
        self.d.click(x, y)

    def tap_element(self, locators: list[Locator]) -> Locator:
        """按顺序尝试多个定位，返回实际生效的那个。"""
        for loc in locators:
            if loc.by == "point":
                x, y = loc.value
                self.tap(int(x), int(y))
                return loc
            if loc.by == "description":
                el = self.d(description=loc.value)
            elif loc.by == "description_contains":
                el = self.d(descriptionContains=loc.value)
            elif loc.by == "text":
                el = self.d(text=loc.value)
            elif loc.by == "resource_id":
                el = self.d(resourceId=loc.value)
            elif loc.by == "xpath":
                el = self.d.xpath(loc.value)
            else:  # pragma: no cover
                continue
            if el.exists:
                el.click()
                return loc
        # 全部失效，抛错交给上层判定
        raise LookupError(f"所有定位都失效: {[l.to_dict() for l in locators]}")

    def input_text(self, text: str, clear: bool = True) -> None:
        """输入文本；默认先清空输入框（房间名等字段会预填旧值）。"""
        if clear:
            self.clear_focused_input()
        # Flutter 输入框用 send_keys 更稳
        self.d.send_keys(text, clear=False)
        self.hide_keyboard()

    def keyboard_shown(self) -> bool:
        out = self.d.shell("dumpsys input_method").output
        return "mInputShown=true" in out

    def hide_keyboard(self) -> None:
        """输入后收起软键盘：键盘会挡住下方按钮，节点还会塞满元素列表。"""
        time.sleep(0.4)
        if self.keyboard_shown():
            self.d.press("back")
            time.sleep(0.6)

    def clear_focused_input(self, max_chars: int = 60) -> None:
        """清空当前聚焦的输入框。

        u2 自带的 clear=True 在 Flutter 输入框里不生效（只弹「已复制」），
        所以用按键：光标移到末尾，再连发退格键。
        """
        self.d.shell("input keyevent KEYCODE_MOVE_END")
        self.d.shell("input keyevent " + " ".join(["KEYCODE_DEL"] * max_chars))

    def swipe(self, direction: str, scale: float = 0.6) -> None:
        w, h = self.window_size()
        cx, cy = w // 2, h // 2
        dist = int(min(w, h) * scale / 2)
        mapping = {
            "up": (cx, cy + dist, cx, cy - dist),
            "down": (cx, cy - dist, cx, cy + dist),
            "left": (cx + dist, cy, cx - dist, cy),
            "right": (cx - dist, cy, cx + dist, cy),
        }
        if direction not in mapping:
            raise ValueError(f"不支持的滑动方向: {direction}")
        sx, sy, ex, ey = mapping[direction]
        self.d.swipe(sx, sy, ex, ey, duration=0.4)

    def back(self) -> None:
        self.d.press("back")

    def drag(self, x1: int, y1: int, x2: int, y2: int, duration: float = 0.8) -> None:
        """按住起点慢速拖到终点：duration 足够长，Flutter 滑块才会识别为拖动而不是点击。"""
        self.d.swipe(x1, y1, x2, y2, duration=duration)


def describe_elements(elements: list[Element]) -> str:
    """把控件列表渲染成给模型看的精简文本。"""
    lines = []
    for i, el in enumerate(elements, 1):
        label = el.description or el.text or ""
        label = label.replace("\n", " / ")
        if not label:
            label = el.class_name.split(".")[-1]
        flags = "可点" if el.clickable else "只读"
        lines.append(f"[{i}] {label}  ({flags}) {el.bounds}")
    return "\n".join(lines)
