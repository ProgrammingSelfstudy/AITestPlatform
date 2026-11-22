"""在独立 pytest 会话中检查夹具、失败采集和生成脚本的导入。"""
import json
import os
import shutil
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

AUTOMATION_ROOT = Path(__file__).resolve().parents[1]
USER = "假账号@example.invalid"
PASSWORD = "假密码'\\\n后半段"

FAKE_DEVICE_CONFTEST = '''
import json
import sys
from pathlib import Path
from types import SimpleNamespace

from hafa_automation import pytest_plugin, ui
from hafa_automation.device.base import Element

pytest_plugins = ["hafa_automation.pytest_plugin"]


def event(*fields):
    with Path("事件.jsonl").open("a", encoding="utf-8") as log:
        log.write(json.dumps(fields, ensure_ascii=False) + "\\n")


class Device:
    active = False

    def elements(self, retry=None):
        if not self.active:
            return []
        return [Element(0, "失败页面：" + USER, "", "", "View", True, (0, 200, 300, 300)),
                Element(1, "", PASSWORD, "", "View", False, (0, 300, 300, 400))]

    def screenshot(self):
        event("截图", self.active)
        return b"fake-failure-png" if self.active else b"fake-home-png"


device = ui.dev = Device()
real_ui = pytest_plugin.Ui


def make_ui():
    event("创建 Ui")
    return real_ui()


def setup_process(command, *, input, capture_output, encoding, errors, env):
    assert command[:3] == [sys.executable, "-m", "hafa_automation"]
    assert capture_output and encoding == "utf-8"
    assert all(USER not in value and PASSWORD not in value for value in (*command, *env.values()))
    args = command[3:]
    if args == ["--teardown"]:
        assert input == ""
        event("关房")
        device.active = False
        return SimpleNamespace(returncode=0, stdout="", stderr="")
    assert args[0] == "--case"
    if env.get("HAFA_ACCOUNT_ID"):
        assert args[2:] == ["--account-id", env["HAFA_ACCOUNT_ID"], "--credentials-stdin"]
        assert json.loads(input) == {"username": USER, "password": PASSWORD}
    else:
        assert args[2:] == ["--skip-login"] and input == ""
    event("准备", args[1])
    device.active = True
    if env.get("FAKE_SETUP_FAILURE"):
        return SimpleNamespace(returncode=1, stdout="失败 " + USER, stderr=repr(PASSWORD))
    return SimpleNamespace(returncode=0, stdout="", stderr="")


pytest_plugin.Ui = make_ui
pytest_plugin.subprocess = SimpleNamespace(run=setup_process)
'''


@pytest.fixture
def 脚本目录(tmp_path):
    generated = tmp_path / "generated"
    generated.mkdir()
    (generated / "__init__.py").touch()
    (generated / "conftest.py").write_text(
        f"USER = {USER!r}\nPASSWORD = {PASSWORD!r}\n" + FAKE_DEVICE_CONFTEST, encoding="utf-8"
    )
    return generated


def _write_script(directory: Path, script: str) -> None:
    (directory / "test_example.py").write_text(textwrap.dedent(script), encoding="utf-8")


def _pytest(directory: Path, *args: str, stdin: str = "", **overrides: str) -> subprocess.CompletedProcess:
    env = os.environ.copy()
    for key in ("PYTEST_ADDOPTS", "PYTEST_PLUGINS", "HAFA_ACCOUNT_ID", "HAFA_ACCOUNT_DISPLAY_NAME", "HAFA_ARTIFACT_DIR"):
        env.pop(key, None)
    env.update(PYTHONPATH=str(AUTOMATION_ROOT), PYTEST_DISABLE_PLUGIN_AUTOLOAD="1", **overrides)
    return subprocess.run([sys.executable, "-m", "pytest", "-q", "--strict-markers", *args, "generated"],
                          cwd=directory.parent, env=env, input=stdin, capture_output=True,
                          encoding="utf-8", timeout=30)


def _events(directory: Path) -> list[list]:
    return [json.loads(line) for line in (directory.parent / "事件.jsonl").read_text(encoding="utf-8").splitlines()]


def test_插件按标记准备和关房且会话夹具只暴露租约资料(脚本目录):
    _write_script(脚本目录, '''
        import pytest

        @pytest.mark.case("TC-04")
        def test_甲(ui, account):
            assert ui.dev.active
            assert account == {"lease_id": "lease-04", "display_name": "回归账号"}

        def test_无标记(ui):
            assert not ui.dev.active

        @pytest.mark.case("TC-06c-F")
        def test_乙(ui, account):
            assert ui.dev.active
            assert set(account) == {"lease_id", "display_name"}
    ''')
    result = _pytest(脚本目录, stdin=json.dumps({"username": USER, "password": PASSWORD}),
                     HAFA_ACCOUNT_ID="lease-04", HAFA_ACCOUNT_DISPLAY_NAME="回归账号")
    assert result.returncode == 0, result.stdout + result.stderr
    assert _events(脚本目录) == [["创建 Ui"], ["准备", "TC-04"], ["关房"], ["准备", "TC-06c-F"], ["关房"]]


def test_用例失败先脱敏存档再关房(脚本目录, tmp_path):
    _write_script(脚本目录, '''
        import pytest

        @pytest.mark.case("TC-04")
        def test_断言失败(ui):
            print(ui.labels())
            assert False, ui.labels()
    ''')
    artifacts = tmp_path / "现场"
    result = _pytest(脚本目录, stdin=json.dumps({"username": USER, "password": PASSWORD}),
                     HAFA_ACCOUNT_ID="lease-04", HAFA_ARTIFACT_DIR=str(artifacts))
    assert result.returncode == 1, result.stdout + result.stderr
    assert _events(脚本目录) == [["创建 Ui"], ["准备", "TC-04"], ["截图", True], ["关房"]]
    (screenshot,) = artifacts.glob("TC-04-*-call-screen.png")
    assert screenshot.read_bytes() == b"fake-failure-png"
    (elements,) = artifacts.glob("TC-04-*-call-elements.txt")
    tree = elements.read_text(encoding="utf-8")
    normalized = PASSWORD.replace("\n", " / ")
    for text in (tree, result.stdout, result.stderr):
        assert USER not in text and PASSWORD not in text and normalized not in text
        assert repr(normalized)[1:-1] not in text
    assert "失败页面：{{account.username}}" in tree and "{{account.password}}" in tree
    assert "(0, 300, 300, 400)" in tree


def test_前置失败也先采集再关房且不回显凭证(脚本目录, tmp_path):
    _write_script(脚本目录, '''
        import pytest

        @pytest.mark.case("TC-04")
        def test_前置失败不进入用例():
            pytest.fail("不应执行用例本体")
    ''')
    artifacts = tmp_path / "现场"
    result = _pytest(脚本目录, stdin=json.dumps({"username": USER, "password": PASSWORD}),
                     HAFA_ACCOUNT_ID="lease-04", HAFA_ARTIFACT_DIR=str(artifacts), FAKE_SETUP_FAILURE="1")
    out = result.stdout + result.stderr
    assert result.returncode == 1 and "准备前置状态失败" in out
    assert "不应执行用例本体" not in out and USER not in out and repr(PASSWORD) not in out
    assert "{{account.username}}" in out and "{{account.password}}" in out
    assert _events(脚本目录) == [["创建 Ui"], ["准备", "TC-04"], ["截图", True], ["关房"]]
    assert len(list(artifacts.glob("TC-04-*-setup-*"))) == 2


def test_未设存档目录不保存且无租约沿用跳过登录(脚本目录):
    _write_script(脚本目录, '''
        import pytest

        @pytest.mark.case("TC-04")
        def test_失败(account):
            assert account == {"lease_id": None, "display_name": None}
            pytest.fail("模拟失败")
    ''')
    result = _pytest(脚本目录, stdin="无租约时不得读取 stdin")
    assert result.returncode == 1 and "模拟失败" in result.stdout
    assert _events(脚本目录) == [["准备", "TC-04"], ["关房"]]
    assert not list(脚本目录.parent.rglob("TC-04-*"))


def test_有租约却未收到有效凭证时不执行前置(脚本目录):
    _write_script(脚本目录, '''
        import pytest

        @pytest.mark.case("TC-04")
        def test_不应进入用例():
            pytest.fail("不应执行用例本体")
    ''')
    result = _pytest(脚本目录, stdin="无效数据" + PASSWORD, HAFA_ACCOUNT_ID="lease-04")
    out = result.stdout + result.stderr
    assert result.returncode == 1 and "须从 stdin 一次性传入有效的账号凭证 JSON" in out
    assert "不应执行用例本体" not in out and "无效数据" not in out and PASSWORD not in out
    assert not (脚本目录.parent / "事件.jsonl").exists()


def test_复制PoC的TC04到临时目录可只收集不执行(脚本目录):
    poc = Path(os.environ.get("HAFA_POC_ROOT") or AUTOMATION_ROOT.parent.parent / "AIAutomation") / "generated"
    sources = (poc / "test_tc04.py", poc / "pages" / "tc04_pages.py")
    if not all(source.is_file() for source in sources):
        pytest.skip("未找到相邻 PoC 仓库；可通过 HAFA_POC_ROOT 指定其路径")
    pages = 脚本目录 / "pages"
    pages.mkdir()
    (pages / "__init__.py").touch()
    shutil.copyfile(sources[0], 脚本目录 / sources[0].name)
    shutil.copyfile(sources[1], pages / sources[1].name)

    result = _pytest(脚本目录, "--collect-only", stdin="收集时不得读取凭证", HAFA_ACCOUNT_ID="lease-04")
    assert result.returncode == 0, result.stdout + result.stderr
    assert "generated/test_tc04.py::test_tc04_swipe_find_and_add_song" in result.stdout
    assert not (脚本目录.parent / "事件.jsonl").exists()
