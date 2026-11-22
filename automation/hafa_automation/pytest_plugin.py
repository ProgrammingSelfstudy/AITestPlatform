"""生成脚本的夹具；用和 Rust 节点一致的入口准备设备及登录。"""
from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
from functools import partial
from pathlib import Path

import pytest

from .runtime import Ui

_CREDENTIALS = pytest.StashKey[dict[str, str] | None]()
_ARTIFACT_UI = pytest.StashKey[Ui]()
_CASE_ID = re.compile(r"TC-[A-Za-z0-9][A-Za-z0-9-]{0,63}")


def pytest_configure(config):
    config.addinivalue_line("markers", "case(id): 由用例号自动准备前置状态并在用例后关房")


def pytest_unconfigure(config):
    if _CREDENTIALS in config.stash:
        del config.stash[_CREDENTIALS]


def _case_id(item) -> str | None:
    marker = item.get_closest_marker("case")
    if marker and len(marker.args) == 1 and not marker.kwargs:
        case = marker.args[0]
        if isinstance(case, str) and _CASE_ID.fullmatch(case):
            return case
    return None


def _read_credentials(config) -> None:
    """一个 pytest 会话只读一次 stdin；凭证不作为夹具暴露。"""
    if _CREDENTIALS not in config.stash:
        capture = config.pluginmanager.get_plugin("capturemanager")
        if capture is not None:
            capture.suspend_global_capture(in_=True)
        try:
            data = None if sys.stdin.isatty() else json.loads(sys.stdin.read())
        except (OSError, ValueError):
            data = None
        finally:
            if capture is not None:
                capture.resume_global_capture()
        keys = ("username", "password")
        valid = isinstance(data, dict) and all(isinstance(data.get(key), str) and data[key] for key in keys)
        config.stash[_CREDENTIALS] = {key: data[key] for key in keys} if valid else None
    if config.stash[_CREDENTIALS] is None:
        pytest.fail("已指定账号租约，须从 stdin 一次性传入有效的账号凭证 JSON", pytrace=False)


def _redact(text: str, credentials: dict[str, str] | None) -> str:
    """按最长值优先替换，同时覆盖控件树换行和异常中的转义形式。"""
    if not credentials:
        return text
    replacements = {}
    for key in ("password", "username"):
        value = credentials[key]
        flattened = value.replace("\n", " / ")
        for form in (value, flattened, flattened.strip()):
            if not form:
                continue
            for variant in (form, repr(form)[1:-1], json.dumps(form, ensure_ascii=False)[1:-1],
                            json.dumps(form)[1:-1]):
                replacements.setdefault(variant, "{{account.%s}}" % key)
    pattern = "|".join(re.escape(value) for value in sorted(replacements, key=len, reverse=True))
    return re.sub(pattern, lambda match: replacements[match.group()], text)


def _run_setup(config, args: list[str], *, send_credentials: bool = False) -> None:
    credentials = config.stash.get(_CREDENTIALS, None)
    payload = json.dumps(credentials) if send_credentials else ""
    module_root = str(Path(__file__).resolve().parents[1])
    pythonpath = os.pathsep.join(filter(None, (module_root, os.environ.get("PYTHONPATH"))))
    phase = "关房" if args == ["--teardown"] else "准备前置状态"
    try:
        result = subprocess.run([sys.executable, "-m", "hafa_automation", *args], input=payload,
                                capture_output=True, encoding="utf-8", errors="replace",
                                env={**os.environ, "PYTHONPATH": pythonpath})
    except OSError as error:
        detail = _redact(str(error), credentials)
        pytest.fail(f"{phase}失败：{detail}", pytrace=False)
    if result.returncode != 0:
        detail = _redact(result.stdout + "\n" + result.stderr, credentials).strip()[-1500:]
        pytest.fail(f"{phase}失败（退出码 {result.returncode}）：\n{detail}", pytrace=False)


@pytest.fixture(scope="session")
def ui() -> Ui:
    return Ui()


@pytest.fixture
def account(pytestconfig) -> dict[str, str | None]:
    """仅返回租约 id 和独立的非敏感展示名，不读取账号密码作为展示名。"""
    display_name = os.environ.get("HAFA_ACCOUNT_DISPLAY_NAME")
    if display_name is not None:
        display_name = _redact(display_name, pytestconfig.stash.get(_CREDENTIALS, None))
    return {"lease_id": os.environ.get("HAFA_ACCOUNT_ID"), "display_name": display_name}


@pytest.fixture(autouse=True)
def _case_env(request):
    if request.node.get_closest_marker("case") is None:
        return
    case = _case_id(request.node)
    if case is None:
        pytest.fail('请用 @pytest.mark.case("TC-xx") 指定一个有效的用例号', pytrace=False)

    lease_id = os.environ.get("HAFA_ACCOUNT_ID")
    args = ["--case", case]
    if lease_id:
        _read_credentials(request.config)
        args.extend(["--account-id", lease_id, "--credentials-stdin"])
    else:
        args.append("--skip-login")
    if os.environ.get("HAFA_ARTIFACT_DIR"):
        request.node.stash[_ARTIFACT_UI] = request.getfixturevalue("ui")

    # 即使前置失败，也要在失败采集完成后尝试关掉可能已经打开的房间。
    request.addfinalizer(partial(_run_setup, request.config, ["--teardown"]))
    _run_setup(request.config, args, send_credentials=bool(lease_id))


def _save_failure(item, report, credentials: dict[str, str] | None) -> None:
    directory = os.environ.get("HAFA_ARTIFACT_DIR")
    case = _case_id(item)
    driver = item.stash.get(_ARTIFACT_UI, None)
    if not directory or case is None or driver is None:
        return

    from .device.android import describe_elements

    path = Path(directory)
    unique = hashlib.sha256(item.nodeid.encode("utf-8")).hexdigest()[:12]
    prefix = f"{case}-{unique}-{report.when}"
    captures = (
        ("screen.png", lambda: driver.dev.screenshot()),
        ("elements.txt", lambda: _redact(describe_elements(driver.elements()), credentials).encode("utf-8")),
    )
    for suffix, collect in captures:
        try:
            path.mkdir(parents=True, exist_ok=True)
            (path / f"{prefix}-{suffix}").write_bytes(collect())
        except Exception as error:
            detail = _redact(str(error), credentials)
            report.sections.append(("失败现场采集", f"无法保存 {suffix}：{detail}"))


@pytest.hookimpl(wrapper=True, tryfirst=True)
def pytest_runtest_makereport(item):
    report = yield
    credentials = item.config.stash.get(_CREDENTIALS, None)
    if credentials:
        report.sections = [(name, _redact(text, credentials)) for name, text in report.sections]
        if report.failed:
            detail = report.longreprtext
            sanitized = _redact(detail, credentials)
            if sanitized != detail:
                report.longrepr = sanitized
    if report.failed:
        _save_failure(item, report, credentials)
    return report
