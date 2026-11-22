"""setup 共用的 UI 小工具（不经过模型，直接用设备层）。

按 content-desc 找元素，找不到才报错，不猜坐标。
配置全部来自环境变量，由 Rust 节点在调起时设置；这里不读任何配置文件。
"""
from __future__ import annotations

import os
import sys
import time

from .device.android import MAX_DUMP_ATTEMPTS, AndroidDevice
from .device.base import Device, Element


def _env(name: str, default: str | None = None) -> str:
    v = os.environ.get(name, default)
    if v is None or v == "":
        print(f"缺少环境变量 {name}", file=sys.stderr)
        raise SystemExit(2)
    return v


SERIAL = _env("HAFA_SERIAL")
PKG = _env("HAFA_APP_PACKAGE")
ACT = _env("HAFA_APP_ACTIVITY")
# 底部坐标的设备偏移：带三键导航栏的机型底部内容上移；主测试机为 0
BOTTOM_SHIFT = int(os.environ.get("HAFA_BOTTOM_SHIFT", "0") or 0)
dev = AndroidDevice(SERIAL)


def normalize_label(text: str) -> str:
    return (text or "").replace("\n", " / ")


def label(element: Element, *, description_only: bool = False) -> str:
    return normalize_label(element.description if description_only else element.description or element.text)


def elements(retry: int = 4, *, device: Device | None = None) -> list[Element]:
    """过滤状态栏；空树重试由设备层统一处理。"""
    source = dev if device is None else device
    observed = source.elements() if retry >= MAX_DUMP_ATTEMPTS else source.elements(retry)
    return [element for element in observed if not element.in_status_bar]


def page_labels() -> list[str]:
    return [label(e) for e in elements() if label(e)]


def page_text() -> str:
    return "\n".join(page_labels())


def has(text: str) -> bool:
    return text in page_text()


def wait_for(text: str, timeout: float = 10) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        if has(text):
            return True
        time.sleep(1)
    return False


def find(text: str, exact: bool = False, *, candidates: list[Element] | None = None,
         clickable: bool | None = None, description_only: bool = False) -> Element | None:
    """返回第一个 content-desc 命中的元素；优先可点击的。"""
    if description_only:
        text = normalize_label(text).strip()
    hits = []
    for element in elements() if candidates is None else candidates:
        if clickable is not None and element.clickable != clickable:
            continue
        candidate = label(element, description_only=description_only)
        if description_only:
            candidate = candidate.strip()
            if not candidate:
                continue
        if candidate == text if exact else text in candidate:
            hits.append(element)
    return min(hits, key=lambda element: not element.clickable, default=None)


def tap_text(text: str, exact: bool = False, wait: float = 1.5):
    e = find(text, exact)
    if e is None:
        raise LookupError(f"页面上找不到「{text}」")
    dev.tap(*e.center)
    time.sleep(wait)
    return e


def tap_xy(x: int, y: int, wait: float = 1.5, *, device: Device | None = None):
    source = dev if device is None else device
    source.tap(x, y)
    time.sleep(wait)
