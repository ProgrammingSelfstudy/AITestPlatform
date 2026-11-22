"""验证静态契约直接来自运行时签名，且可无设备配置读取。"""
import inspect
import json
import os
import subprocess
import sys
from pathlib import Path

from hafa_automation.runtime import Ui
from hafa_automation.script_contract import ALLOWED_IMPORTS, UI_API


def test_契约JSON与Ui签名一致且不需要设备环境变量():
    names = """elements labels find has read wait_text wait_gone wait_any tap_xy tap scroll_to type_after
               drag swipe back restart_app wait debug_overlay_up dismiss_debug_overlay close_popups""".split()
    signatures = {name: list(inspect.signature(getattr(Ui, name)).parameters)[1:] for name in names}
    assert UI_API == signatures
    assert {"pytest", "re", "time", "hafa_automation.*", "generated.pages.*"} <= set(ALLOWED_IMPORTS)

    env = {key: value for key, value in os.environ.items() if not key.startswith("HAFA_")}
    result = subprocess.run([sys.executable, "-m", "hafa_automation.script_contract", "--json"],
                            cwd=Path(__file__).resolve().parents[1], env=env, check=True,
                            capture_output=True, encoding="utf-8")
    assert json.loads(result.stdout) == {"ui_api": signatures, "allowed_imports": ALLOWED_IMPORTS}
