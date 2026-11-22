"""用假设备验证生成脚本运行时，不连接真机。"""
from types import SimpleNamespace

import pytest

from hafa_automation.device import android
from hafa_automation.device.base import Device, Locator
from hafa_automation.runtime import Ui
from tests.conftest import FakeDev, el


class ScriptDevice(FakeDev):
    tap_input = Device.tap_input

    def __init__(self, pages, start, on_tap=None, on_swipe=None):
        super().__init__(pages, start, on_tap=on_tap)
        self.on_swipe = on_swipe or (lambda page: page)
        self.swipes, self.drags, self.app_calls, self.locators = [], [], [], []

    def swipe(self, direction, scale=0.6):
        self.swipes.append(direction)
        self.cur = self.on_swipe(self.cur)

    def drag(self, *args):
        self.drags.append(args)

    def stop_app(self, package):
        self.app_calls.append(("stop", package))

    def launch_app(self, package, activity=None):
        self.app_calls.append(("launch", package, activity))

    def tap_element(self, locators):
        self.locators.extend(locators)
        return locators[0]


def test_页面观察支持原生文字但定位只用描述(install):
    status = el("时间", (0, 0, 90, 90))
    readonly = el("  曲目\n00:30 ", clickable=False)
    native = el("")
    native.text = "原生提示"
    target = el("曲目 / 00:30")
    install(FakeDev({"页面": [status, readonly, native, target]}, "页面"))
    driver = Ui()

    assert driver.elements() == [readonly, native, target]
    assert driver.labels() == ["曲目 / 00:30", "原生提示", "曲目 / 00:30"]
    assert driver.find("曲目\n00:30") is target
    assert driver.find("曲目", exact=False, clickable=False) is readonly
    assert driver.find("曲目") is None
    assert driver.find("原生提示") is None
    assert driver.has("原生") and not driver.has("原生", exact=True)
    assert driver.read(r"\d\d:\d\d$") == "曲目 / 00:30"
    assert driver.read(r"^\d+%$") is None


def test_点击先描述再坐标并保留命中范围内的原坐标(install):
    target = el("按钮", (100, 200, 300, 400))
    native = el("", (300, 600, 500, 800))
    native.text = "仅原生文字"
    device = install(ScriptDevice({"起点": [target, native]}, "起点"))
    driver = Ui()

    driver.tap("按钮", xy=(330, 500))
    driver.tap("按钮", xy=(110, 375))
    driver.tap("仅原生文字", xy=(120, 510))
    driver.tap_xy(340, 500)
    assert device.taps == [target.center, (110, 375), (120, 510), (340, 500)]
    with pytest.raises(AssertionError, match="找不到.*不存在"):
        driver.tap("不存在")


def test_XPath只在描述和坐标都不可用时使用(install):
    xpath = "//*[@content-desc='后备']"
    target = el(xpath, (100, 200, 300, 400))
    device = install(ScriptDevice({"起点": [target], "后续": []}, "起点"))
    driver = Ui()
    driver.tap(xpath, xy=(20, 500))
    assert device.taps == [target.center]

    device.cur = "后续"
    driver.tap(xpath, xy=(20, 500))
    assert device.taps[-1] == (20, 500)
    assert device.locators == []
    driver.tap(xpath)
    assert device.locators == [Locator("xpath", xpath)]


def test_等待文字按虚拟时钟超时并报告页面(install, clock):
    install(FakeDev({"页面": [el("正在加载")]}, "页面"))
    driver = Ui()
    start = clock[0]
    with pytest.raises(AssertionError, match="等待超时.*加载.*正在加载"):
        driver.wait_text("加载", timeout=1, exact=True)
    assert clock[0] - start == 1
    driver.wait_text("加载", timeout=1)


def test_等待任一候选和等待消失(install, clock, monkeypatch):
    device = install(FakeDev({"开始": [el("加载中")], "完成": [el("可播放"), el("音乐")]}, "开始"))
    start = clock[0]
    original_elements = device.elements

    def elements():
        device.cur = "完成" if clock[0] >= start + 1 else "开始"
        return original_elements()

    monkeypatch.setattr(device, "elements", elements)
    driver = Ui()
    driver.wait_gone("加载", timeout=2)
    assert clock[0] - start == 1
    assert driver.wait_any(["未出现", "音乐", "可播放"]) == "音乐"
    with pytest.raises(AssertionError, match="没有消失"):
        driver.wait_gone("音乐", timeout=0.5)
    with pytest.raises(AssertionError, match="都没有出现.*当前页面文字"):
        driver.wait_any(["未出现"], timeout=0.5)


@pytest.mark.parametrize("max_swipes", [0, 2])
def test_滚动绝不超过上限(install, max_swipes):
    pages = {index: [el(f"第 {index} 页")] for index in range(4)}
    pages[3].append(el("目标"))
    device = install(ScriptDevice(pages, 0, on_swipe=lambda page: page + 1))
    assert Ui().scroll_to("目标", max_swipes=max_swipes) is None
    assert device.swipes == ["up"] * max_swipes


def test_滚动匹配完整可见的行且到末尾就停止(install):
    visible = el("歌曲 / 00:30", (0, 200, 720, 320))
    pages = {"起点": [el("歌曲 / 00:30", (0, 1450, 720, 1570))], "终点": [visible, el("末尾")]}
    device = install(ScriptDevice(pages, "起点", on_swipe=lambda page: "终点"))
    driver = Ui()
    driver.tap("歌曲 / ", exact=False, scroll=True)
    assert device.taps == [visible.center]
    assert device.swipes == ["up"]
    assert driver.scroll_to("不存在") is None
    assert device.swipes == ["up", "up"]


@pytest.mark.parametrize(("retry", "attempts", "populated"), [(0, 0, False), (1, 1, False), (4, 3, True), (20, 3, False)])
def test_空控件树由设备层重试且最多请求三次(monkeypatch, retry, attempts, populated):
    monkeypatch.setenv("HAFA_SERIAL", "FAKE-RUNTIME-SERIAL")
    calls = []

    def dump(compressed, max_depth):
        calls.append((compressed, max_depth))
        if populated and len(calls) == 3:
            return '<hierarchy><node content-desc="已恢复" bounds="[0,200][200,300]"/></hierarchy>'
        return "<hierarchy/>"

    def connect(serial):
        assert serial == "FAKE-RUNTIME-SERIAL"
        return SimpleNamespace(settings={"max_depth": 50}, jsonrpc=SimpleNamespace(dumpWindowHierarchy=dump))

    monkeypatch.setattr(android.u2, "connect", connect)
    driver = Ui()
    assert driver.serial == "FAKE-RUNTIME-SERIAL"
    assert [element.description for element in driver.elements(retry=retry)] == (["已恢复"] if populated else [])
    assert calls == [(False, 50)] * attempts


def test_输入定位到描述后的输入框或使用坐标(install):
    field = el("旧内容", (100, 220, 300, 410))
    native = el("", clickable=False)
    native.text = "原生标题"
    device = install(ScriptDevice({"页面": [el("房间名", clickable=False), native, field]}, "页面"))
    driver = Ui()
    driver.type_after("房间名", "新内容", xy=(300, 500))
    driver.type_after("原生标题", "第二次", xy=(300, 500))
    assert device.typed == ["新内容", "第二次"]
    assert device.taps == [field.input_point, (300, 500)]
    with pytest.raises(AssertionError, match="原生标题.*输入框"):
        driver.type_after("原生标题", "不应输入")
    assert len(device.typed) == 2


def test_拖动滑动返回重启和显式等待转发给设备(install, clock):
    device = install(ScriptDevice({"页面": []}, "页面"))
    driver = Ui()
    start = clock[0]
    driver.drag(120, 240, 360, 480, duration=1.2, wait=0.1)
    driver.swipe("down", wait=0.2)
    driver.back(wait=0.3)
    driver.restart_app(wait=2)
    driver.wait(0.4)
    assert device.drags == [(120, 240, 360, 480, 1.2)]
    assert device.swipes == ["down"] and device.backs == 1
    assert device.app_calls == [("stop", driver.pkg), ("launch", driver.pkg, driver.act)]
    assert clock[0] - start == pytest.approx(4.5)


def test_调试面板关闭最多尝试三次(install):
    device = install(ScriptDevice({"面板": [el("Close", (0, 0, 720, 1600))]}, "面板"))
    driver = Ui()
    assert driver.debug_overlay_up() and driver.dismiss_debug_overlay()
    assert device.taps == [(668, 1005)] * 3


def test_关闭弹窗按调试面板活动签到顺序且不签到(install):
    sign_in = el("Close sign-in", (310, 1100, 410, 1200))
    pages = {"面板": [el("Close", (0, 0, 720, 1600))], "活动": [el("Close activities")],
             "签到": [sign_in], "首页": [el("Close")]}
    transitions = {("面板", 668, 1005): "活动", ("活动", 360, 1265): "签到", ("签到", *sign_in.center): "首页"}
    device = install(ScriptDevice(pages, "面板", on_tap=lambda page, x, y, dev: transitions[page, x, y]))
    driver = Ui()
    driver.close_popups()
    assert device.cur == "首页"
    assert not driver.debug_overlay_up() and not driver.dismiss_debug_overlay()
    assert device.taps == [(668, 1005), (360, 1265), sign_in.center]
