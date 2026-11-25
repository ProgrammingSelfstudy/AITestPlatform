"""每条用例的前置状态规格。setup_env --case 按它准备手机。

- library：个人音乐库应有的歌曲（文件名不含扩展名）；[] 表示空库
- room：是否预先开好房间（True 时 setup 结束后手机停在房间内）
用例 YAML 只读，所以规格放在这里，并和用例里的 preconditions 对应。
"""
from __future__ import annotations

import os

SPECS: dict[str, dict] = {
    "TC-01":   {"library": [], "room": False},   # 用例自己开房
    "TC-02":   {"library": ["tst_normal_01"], "room": True},
    "TC-02-F": {"library": ["tst_normal_01"], "room": True},
    "TC-03":   {"library": [], "room": True},
    "TC-04":   {"library": [], "room": True},
    "TC-05":   {"library": ["tst_normal_01"], "room": True},
    "TC-06a":  {"library": [], "room": True},
    "TC-06a-F": {"library": [], "room": True},
    "TC-06b":  {"library": [], "room": True},
    "TC-06b-F": {"library": [], "room": True},
    "TC-06c":  {"library": [], "room": True},
    "TC-06c-F": {"library": [], "room": True},
    "TC-07":   {"library": ["tst_normal_01"], "room": True},
    # 房间音量按账号保存、跨次保留；用例前置要求「未调整过」，所以每次 setup 都把音量复位到默认 50%
    "TC-08":   {"library": ["tst_normal_01"], "room": True, "volume": 50},
    "TC-09":   {"library": ["tst_normal_01"], "room": True},
    # 用例预期里有「其他歌曲记录不受影响」，库里必须还有别的歌才能验证，所以比 YAML 前置多放一首
    "TC-10":   {"library": ["tst_normal_del", "tst_normal_01"], "room": True},
}
DEFAULT = {"library": [], "room": True}


def spec_for(case_id: str) -> dict:
    return SPECS.get(case_id, DEFAULT)


def has_spec(case_id: str) -> bool:
    """是否有该用例的专属 Hafa 语聊房前置规格；新模块生成的 GEN-* 等用例不在表里，走通用准备。"""
    return case_id in SPECS


# 无人值守时用来「模拟人工」的动作。真人场景用 run.py --human prompt 在终端处理。
# 这只是为了让流程能自动跑通，结果里会标注 human_mode=sim，不能当作「真人介入已验证」。
def _sim_delete_del_song() -> str:
    import subprocess

    serial = os.environ["HAFA_SERIAL"]   # 当前进程操作的那台手机
    subprocess.run(["adb", "-s", serial, "shell", "rm", "-f", "/sdcard/Music/tst_normal_del.mp3"], check=True)
    subprocess.run(["adb", "-s", serial, "shell", "am", "broadcast", "-a",
                    "android.intent.action.MEDIA_SCANNER_SCAN_FILE", "-d", "file:///sdcard/Music"],
                   capture_output=True)
    return "模拟人工：已用 adb 删除 /sdcard/Music/tst_normal_del.mp3 并触发媒体扫描"


HUMAN_SIM = {"TC-10": _sim_delete_del_song}
