"""测试夹具：假设备 + 虚拟时钟。配置从环境变量来，必须在导入包之前设好。"""
import os
import tempfile
from pathlib import Path

_TMP = Path(tempfile.mkdtemp(prefix="hafa_auto_test_"))
os.environ.setdefault("HAFA_SERIAL", "TESTSERIAL")
os.environ.setdefault("HAFA_APP_PACKAGE", "com.test.app")
os.environ.setdefault("HAFA_APP_ACTIVITY", "com.test.app.Main")
os.environ.setdefault("HAFA_BOTTOM_SHIFT", "0")
os.environ["HAFA_STATE_DIR"] = str(_TMP / "state")
os.environ["HAFA_AUDIO_DIR"] = str(_TMP / "audio")
(_TMP / "state").mkdir()
(_TMP / "audio").mkdir()

import time  # noqa: E402

import pytest  # noqa: E402

from hafa_automation import setup_env, ui  # noqa: E402
from hafa_automation.device.base import Element  # noqa: E402


def el(desc: str, bounds=(0, 200, 720, 300), clickable=True) -> Element:
    return Element(index=0, description=desc, text="", resource_id="", class_name="android.view.View",
                   clickable=clickable, bounds=bounds)


class FakeDev:
    """脚本化的假设备：pages 是 页面名 -> 元素列表；on_tap / on_back 决定下一页。"""

    def __init__(self, pages, start, on_tap=None, on_back=None):
        self.pages, self.cur = pages, start
        self.on_tap = on_tap or (lambda cur, x, y, dev: cur)
        self.on_back = on_back or (lambda cur, dev: cur)
        self.taps, self.typed, self.backs = [], [], 0
        self.launched = 0

    def elements(self):
        return list(self.pages[self.cur])

    def tap(self, x, y):
        self.taps.append((x, y))
        self.cur = self.on_tap(self.cur, x, y, self)

    def back(self):
        self.backs += 1
        self.cur = self.on_back(self.cur, self)

    def swipe(self, direction, scale=0.6):
        pass

    def launch_app(self, package, activity=None):
        self.launched += 1

    def stop_app(self, package):
        pass

    def input_text(self, text, clear=True):
        self.typed.append(text)

    def window_size(self):
        return (720, 1600)


@pytest.fixture(autouse=True)
def clock(monkeypatch):
    """虚拟时钟：sleep 只推进时间，不真等；wait_for 里的超时循环也能立刻走完。"""
    now = [1000.0]
    monkeypatch.setattr(time, "time", lambda: now[0])
    monkeypatch.setattr(time, "sleep", lambda s: now.__setitem__(0, now[0] + s))
    return now


@pytest.fixture
def install(monkeypatch):
    """把假设备装进 ui / setup_env，返回它。"""

    def _install(dev: FakeDev) -> FakeDev:
        monkeypatch.setattr(ui, "dev", dev)
        monkeypatch.setattr(setup_env, "dev", dev)
        return dev

    return _install


@pytest.fixture(autouse=True)
def reset_state(monkeypatch):
    monkeypatch.setattr(setup_env, "CREDS", None)
    setup_env.STATE_FILE.unlink(missing_ok=True)
