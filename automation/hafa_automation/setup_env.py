"""用例前置状态准备（setup）和收尾（teardown）。不让 AI 做，也不进生成脚本的断言。

由 Rust 节点调起：

    python -m hafa_automation --account-id <租约id> --credentials-stdin --case TC-04
    python -m hafa_automation --teardown

契约
- 配置全部来自环境变量：HAFA_SERIAL、HAFA_APP_PACKAGE、HAFA_APP_ACTIVITY、HAFA_BOTTOM_SHIFT、
  HAFA_AUDIO_DIR（测试音频目录）、HAFA_STATE_DIR（登录态缓存目录）；
- 账号密码只从 stdin 收一个 JSON（`--credentials-stdin`）：**不读文件、不进命令行、不进环境变量、不打印**；
  输出里出现的任何文字都会先把账号密码替换成占位符；
- stdout 每行一个 JSON 事件：{"type":"step","ok":true,"msg":...}，最后是 {"type":"done"} 或 {"type":"error"}；
- 退出码：0 成功；1 某一步失败（error 事件里带当前页面文字）；2 用法或环境错误；
  3 需要人工（屏幕仍然锁定）；4 需要账号凭证但没有提供。
界面元素按 content-desc 找；找不到的图标用已验证过的坐标，并在注释里写明原因。
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from . import ui
from .ui import dev, has, tap_text, tap_xy, wait_for

SERIAL = ui.SERIAL
BOTTOM_SHIFT = ui.BOTTOM_SHIFT   # 底部坐标的设备偏移；主测试机为 0
AUDIO_DIR = Path(os.environ["HAFA_AUDIO_DIR"]) if os.environ.get("HAFA_AUDIO_DIR") else None
PHONE_MUSIC = "/sdcard/Music"

# 账号凭证：只在内存里，只由 stdin 填充
CREDS: dict[str, str] | None = None
EXIT_STEP, EXIT_USAGE, EXIT_HUMAN, EXIT_CREDS = 1, 2, 3, 4


def _redact(text: str) -> str:
    """输出前把账号密码替换成占位符（纵深防御：正常流程本来就不会打印它们）。"""
    if not CREDS:
        return text
    for key in ("password", "username"):   # 先密码后账号；太短的值不替换，免得误伤正常文字
        v = CREDS.get(key) or ""
        if len(v) >= 3:
            text = text.replace(v, "{{account.%s}}" % key)
    return text


def emit(kind: str, **data) -> None:
    """输出一个 JSON 事件。所有字符串字段都先脱敏。"""
    def clean(v):
        if isinstance(v, str):
            return _redact(v)
        if isinstance(v, list):
            return [clean(x) for x in v]
        return v
    print(json.dumps({"type": kind, **{k: clean(v) for k, v in data.items()}}, ensure_ascii=False), flush=True)


def _creds() -> dict[str, str]:
    if not CREDS or not CREDS.get("username") or not CREDS.get("password"):
        emit("error", msg="需要登录但没有提供账号凭证（--credentials-stdin）", code="need_credentials")
        raise SystemExit(EXIT_CREDS)
    return CREDS


def step(msg: str, ok: bool = True) -> None:
    emit("step", ok=ok, msg=msg)
    if not ok:
        try:
            page = ui.page_labels()
        except Exception as e:  # noqa: BLE001  现场都读不到时也要报出来
            page = [f"（读取当前页面失败：{type(e).__name__}）"]
        emit("error", msg=f"步骤失败：{msg}", code="step_failed", page=page)
        raise SystemExit(EXIT_STEP)


def adb(*args: str) -> str:
    r = subprocess.run(["adb", "-s", SERIAL, *args], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"adb {' '.join(args)} 失败：{r.stderr.strip()}")
    return r.stdout


# ---------------------------------------------------------------- 屏幕
def ensure_awake() -> None:
    """唤醒并解锁；USB 连接时保持常亮（长时间批量执行中途锁屏会让所有操作落空）。
    设了 PIN / 图案的手机解不开：此时报「需要人工」（退出码 3），不假装成功。"""
    adb("shell", "svc", "power", "stayon", "usb")
    if "mWakefulness=Asleep" in adb("shell", "dumpsys", "power") or "isKeyguardShowing=true" in adb("shell", "dumpsys", "window"):
        adb("shell", "input", "keyevent", "KEYCODE_WAKEUP")
        time.sleep(1)
        adb("shell", "wm", "dismiss-keyguard")
        time.sleep(1.5)
    if "isKeyguardShowing=true" in adb("shell", "dumpsys", "window"):
        emit("step", ok=False, msg="屏幕仍然锁定")
        emit("error", msg="屏幕仍然锁定（可能设了 PIN / 图案），需要人工解锁", code="screen_locked")
        raise SystemExit(EXIT_HUMAN)
    step("屏幕已唤醒并解锁")


# ---------------------------------------------------------------- 测试音频
def push_audio() -> None:
    """缺什么补什么：按文件名和大小比对，只推缺失或大小不一致的。"""
    remote = {}
    for line in adb("shell", f"ls -l {PHONE_MUSIC}").splitlines():
        parts = line.split()
        if len(parts) >= 6 and parts[-1].startswith("tst_"):
            remote[parts[-1]] = int(parts[4])
    pushed = []
    for f in sorted(AUDIO_DIR.iterdir()):
        if f.is_file() and remote.get(f.name) != f.stat().st_size:
            adb("push", str(f), f"{PHONE_MUSIC}/{f.name}")
            pushed.append(f.name)
    if pushed:  # 推送后必须触发媒体扫描，否则 Local Music Library 读不到
        adb("shell", "am", "broadcast", "-a", "android.intent.action.MEDIA_SCANNER_SCAN_FILE",
            "-d", f"file://{PHONE_MUSIC}")
    step(f"测试音频就绪（共 {len(list(AUDIO_DIR.iterdir()))} 个，本次补推 {len(pushed)} 个）")


# ---------------------------------------------------------------- 账号
def debug_overlay_up() -> bool:
    """语音调试面板是带全屏遮罩的弹层（遮罩节点是全屏可点的 Close）。它在时，下面页面的控件不会暴露出来。"""
    return any(ui.label(e) == "Close" and e.bounds == (0, 0, 720, 1600) for e in ui.elements())


def dismiss_debug_overlay() -> bool:
    closed = False
    for _ in range(3):
        if not debug_overlay_up():
            break
        tap_xy(668, 1005, 1.5)   # 面板右上角的 X
        closed = True
    return closed


def close_popups() -> None:
    """关掉首页可能出现的活动弹窗、每日签到弹窗（只关闭，不签到），以及遗留的语音调试面板。"""
    dismiss_debug_overlay()
    for _ in range(4):
        if has("Close activities"):
            tap_xy(360, 1265, 2)  # X 在弹窗正下方，节点中心点不中，用已验证坐标
        elif has("Close sign-in"):
            tap_text("Close sign-in", exact=True, wait=2)
        else:
            break


def go_home() -> None:
    """回到首页（或登录页）。子页面按返回；落在房间里就关房。最多尝试 8 次。"""
    dev.launch_app(ui.PKG, ui.ACT)
    # 冷启动可能要十几秒；页面没加载出来就按返回，会把 App 直接退到桌面
    wait_for_any = [t for t in ("Message / Message", "Welcome to Hafa", "No.2") if wait_for(t, 0)]
    for _ in range(15):
        if wait_for_any or any(has(t) for t in ("Message / Message", "Welcome to Hafa", "No.2", "Voice Party")):
            break
        time.sleep(1)
    for attempt in range(8):
        if attempt == 4:   # 返回键退不出去（比如停在没有 content-desc 的 H5 / vConsole 页）：强停重启一次
            dev.stop_app(ui.PKG)
            time.sleep(2)
            dev.launch_app(ui.PKG, ui.ACT)
            time.sleep(8)
        close_popups()
        if has("Welcome to Hafa"):
            return
        if has("Message / Message"):  # 底部标签栏在 = 已在首页四个 tab 之一（Mine 页没有 Recommend 字样）
            if not has("Recommend"):
                tap_text("Party / Party", exact=True, wait=2)
            return
        if has("No.2") or has("Current player"):
            close_room()
            continue
        dev.back()
        time.sleep(1.5)
    step("回到首页", has("Message / Message") or has("Welcome to Hafa"))


def logout() -> None:
    tap_text("Mine / Mine", exact=True, wait=2)
    step("进入 Mine 页", wait_for("Following"))
    tap_xy(650, 158, 2)  # Settings 节点是整行宽，齿轮在最右侧
    step("进入 Settings", wait_for("Log out"))
    tap_text("Log out", wait=1.5)
    step("出现退出确认", wait_for("log out of this account"))
    tap_text("Confirm", exact=True, wait=4)
    step("已退出，回到登录页", wait_for("Welcome to Hafa"))


def login() -> None:
    acc = _creds()
    if has("Switch to debug environment"):  # 当前是正式环境：只切换，绝不在正式环境登录
        print("⚠️ 登录页在正式环境，切换到 debug 环境")
        tap_text("Switch to debug environment", wait=3)
    step("登录页处于 debug 环境", has("Switch to release environment"))
    tap_text("Email Login", wait=2)
    if has("User agreement"):
        tap_text("I agree", exact=True, wait=2)
    step("进入 Email Login 页", wait_for("Enter your email and password"))
    tap_xy(360, 521, 1)          # 邮箱输入框
    dev.input_text(str(acc["username"]))
    time.sleep(1)
    tap_xy(323, 648, 1)          # 密码输入框
    dev.input_text(str(acc["password"]))
    time.sleep(1)
    # input_text 输入完会自动收起键盘，这里不能再按返回（会退回欢迎页，且欢迎页重建后默认是正式环境）
    tap_xy(360, 849, 3)          # Login；登录有 2～3 秒加载圈
    wait_for("Recommend", 10)
    close_popups()
    step("登录成功，进入首页", wait_for("Recommend", 8))


STATE_FILE = Path(os.environ.get("HAFA_STATE_DIR") or tempfile.gettempdir()) / f".setup_state_{SERIAL}.json"   # 按设备分开：每台手机登录的账号各自记录


def _read_state() -> dict:
    try:
        return json.loads(STATE_FILE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def _write_state(**kw) -> None:
    st = _read_state()
    st.update(kw)
    STATE_FILE.parent.mkdir(exist_ok=True)
    STATE_FILE.write_text(json.dumps(st), encoding="utf-8")


def ensure_account(account_id: str, force: bool = False) -> None:
    """保证当前登录的是 account_id 对应的账号。

    App 界面上看不出登录的是哪个账号，所以用状态文件记住上次 setup 登录的是谁：
    同一个账号且已在首页就不重复退出登录（省约 1 分钟）；状态未知或换账号则重新登录（需要凭证）。
    account_id 是不透明的标识，不是账号本身。
    """
    go_home()
    if not force and has("Message / Message") and _read_state().get("account") == account_id:
        step("已登录目标账号，跳过重新登录")
        return
    if not has("Welcome to Hafa"):
        logout()
    _write_state(account=None)
    login()
    _write_state(account=account_id)


# ---------------------------------------------------------------- 房间
def open_room() -> None:
    if not wait_for("Recommend", 3):
        tap_text("Party / Party", exact=True, wait=2)
    close_popups()
    tap_xy(665, 121, 2)          # 右上角 + 无 content-desc
    step("打开 Voice Party 页", wait_for("Voice Party"))
    tap_text("Start Party", wait=3)
    # 进房后调试面板会弹出并遮住页面的控件树，先关掉它再判断是否进房（最多等 10 秒）
    ok = False
    for _ in range(10):
        dismiss_debug_overlay()
        if has("No.2"):
            ok = True
            break
        time.sleep(1)
    step("开房成功", ok)


def close_room() -> None:
    """房间右上角 … → Close → Confirm。"""
    for _ in range(5):  # 播放器、More 面板、Music/本地库/音量页等会挡住右上角菜单，逐层返回
        if any(has(t) for t in ("Scrim", "Room Tools", "Add Music", "Local Music Library", "Room volume",
                                "Select all", "Manage Seat", "Are you sure")):
            dev.back()
            time.sleep(1.5)
        else:
            break
    if not has("No.2"):
        print("ℹ️ 当前不在房间内，跳过关房")
        return
    tap_xy(657, 129, 2)          # … 菜单图标无 content-desc
    step("房间菜单出现 Close", wait_for("Close"))
    tap_text("Close", exact=True, wait=2)
    step("出现关房确认", wait_for("room will close"))
    tap_text("Confirm", exact=True, wait=4)
    step("关房成功，回到首页", wait_for("Recommend", 8))


# ---------------------------------------------------------------- 音乐库
def open_music_page() -> None:
    tap_xy(295, 1527 + BOTTOM_SHIFT, 2)         # 底部第 2 个图标（九宫格 More）无 content-desc
    step("打开 More 面板", wait_for("Room Tools"))
    tap_text("Music", exact=True, wait=3)
    step("进入 Music 页", wait_for("Add Music"))


def reset_music_library() -> None:
    """保证个人音乐库为空（Music(0/50)）。"""
    open_music_page()
    if has("No music"):
        step("音乐库已为空，无需清理")
    else:
        tap_text("Edit", exact=True, wait=2)
        step("进入管理模式", wait_for("Select all"))
        tap_xy(40, 1533 + BOTTOM_SHIFT, 1.5)    # Select all 的圆圈；文字节点中心点不中
        step("已全选", any(ui.label(e).startswith("Delete") and ui.label(e) != "Delete0" for e in ui.elements()))
        e = next(e for e in ui.elements() if ui.label(e).startswith("Delete"))
        dev.tap(*e.center)
        time.sleep(2)
        step("出现删除确认", has("Are you sure to delete"))
        tap_text("Confirm", exact=True, wait=3)
        step("音乐库已清空", wait_for("No music"))
    dev.back()                   # 回到房间
    time.sleep(1.5)


SONG_ROW = re.compile(r"^(.+?) / \d\d:\d\d$")


def library_songs() -> list[str]:
    """当前 Music 页里的歌曲名（不含时长）。"""
    return sorted(m.group(1) for e in ui.elements() if (m := SONG_ROW.match(ui.label(e))))


def _row_visible(name: str) -> bool:
    """行必须完整露出才算找到：列表边缘的节点可能存在，但只露出 1 像素，点不到。"""
    for e in ui.elements():
        if ui.label(e).startswith(name + " /"):
            l, t, r, b = e.bounds
            return (b - t) >= 100 and t >= 163 and b <= 1487
    return False


def _scroll_to(name: str, max_swipes: int = 10) -> bool:
    for _ in range(max_swipes + 1):
        if _row_visible(name):
            return True
        before = ui.page_text()
        dev.swipe("up")
        time.sleep(1)
        if ui.page_text() == before:
            return False
    return False


def add_songs(names: list[str]) -> None:
    tap_text("Add Music", wait=3)
    step("进入 Local Music Library", wait_for("Local Music Library"))
    for name in names:
        step(f"列表里找到 {name}", _scroll_to(name))
        e = next(e for e in ui.elements() if ui.label(e).startswith(name + " /"))
        dev.tap(*e.center)
        time.sleep(1)
    step(f"底部为 Add({len(names)})", has(f"Add({len(names)})"))
    tap_text(f"Add({len(names)})", wait=3)
    step("回到 Music 页", wait_for(f"Music({len(names)}/50)"))


def ensure_library(songs: list[str]) -> None:
    """让个人音乐库恰好等于 songs。在房间里执行；结束后停在房间页。"""
    open_music_page()
    if library_songs() == sorted(songs):
        step(f"音乐库已是目标状态 {songs or '空'}")
    else:
        if not has("No music"):
            tap_text("Edit", exact=True, wait=2)
            step("进入管理模式", wait_for("Select all"))
            tap_xy(40, 1533 + BOTTOM_SHIFT, 1.5)
            e = next(e for e in ui.elements() if ui.label(e).startswith("Delete"))
            dev.tap(*e.center)
            time.sleep(2)
            step("出现删除确认", has("Are you sure to delete"))
            tap_text("Confirm", exact=True, wait=3)
            step("音乐库已清空", wait_for("No music"))
        if songs:
            add_songs(songs)
            step(f"音乐库已是目标状态 {songs}", library_songs() == sorted(songs))
    dev.back()                   # 回到房间
    time.sleep(1.5)


VOL_LABEL = re.compile(r"^(\d{1,3})%$")
TRACK_L, TRACK_R, TRACK_Y = 44, 676, 1431 + BOTTOM_SHIFT     # Room volume 滑轨在 720×1600 屏上的位置


def current_volume() -> int | None:
    """读当前音量：值标签在滑轨上方（y<1430），0%/100% 是轨道两端的固定范围标签（y≈1461）。"""
    for e in ui.elements():
        m = VOL_LABEL.match(ui.label(e))
        if m and e.bounds[1] < 1430:
            return int(m.group(1))
    return None


def ensure_volume(target: int = 50, tol: int = 1) -> None:
    """把 Room volume 复位到 target%（误差 ≤ tol）：播放一首歌打开音量面板，从「当前圆点位置」拖到目标位置，
    回读后继续修正，最多 8 次；完成后关掉音乐。"""
    open_music_page()
    songs = library_songs()
    step("库里有歌可播放", bool(songs))
    tap_text(songs[0] + " /", wait=2.5)
    tap_text("Room volume", exact=True, wait=2.5)
    step("打开 Room volume", wait_for("0%"))
    def x_of(pct: float) -> int:
        return int(TRACK_L + (TRACK_R - TRACK_L) * pct / 100)

    # 点一下轨道就会把音量设成对应位置，比拖动精确（小幅拖动会低于触摸抖动阈值而不生效）。
    # 读数后按偏差微调再点，最多 8 次。
    aim = float(target)
    for _ in range(8):
        v = current_volume()
        if v is not None and abs(v - target) <= tol:
            break
        if v is not None:
            aim += (target - v)                      # 偏差修正
        tap_xy(min(max(x_of(aim), TRACK_L + 6), TRACK_R - 6), TRACK_Y, 1.2)
    v = current_volume()
    step(f"音量已复位到 {target}%（当前 {v}%）", v is not None and abs(v - target) <= tol)
    tap_text("Turn off music", exact=True, wait=2)        # 关掉音乐，回到「未播放」的初始状态
    step("出现关闭音乐确认", wait_for("Cancel"))
    tap_text("Confirm", exact=True, wait=2.5)
    step("音乐已关闭", not has("Room volume"))


def main(argv: list[str] | None = None) -> int:
    global CREDS
    ap = argparse.ArgumentParser(prog="hafa_automation")
    ap.add_argument("--account-id", help="账号的不透明标识（租约 id），只用来判断是否已登录这个账号")
    ap.add_argument("--credentials-stdin", action="store_true", help='从 stdin 读 {"username":..,"password":..}')
    ap.add_argument("--skip-login", action="store_true", help="不处理登录，假定手机上已经是目标账号")
    ap.add_argument("--teardown", action="store_true", help="只做收尾：关房回首页")
    ap.add_argument("--keep-room", action="store_true", help="setup 后保留房间")
    ap.add_argument("--case", help="按 case_setup.py 里该用例的规格准备前置状态，如 TC-02")
    ap.add_argument("--force-login", action="store_true", help="不信任登录态缓存，强制重新登录")
    args = ap.parse_args(argv)

    if args.credentials_stdin:
        try:
            CREDS = json.loads(sys.stdin.read())
        except ValueError:
            emit("error", msg="stdin 里的凭证不是合法 JSON", code="usage")
            return EXIT_USAGE
    try:
        ensure_awake()
        if args.teardown:
            close_room()
            emit("done", phase="teardown")
            return 0

        if AUDIO_DIR is None:
            emit("error", msg="缺少环境变量 HAFA_AUDIO_DIR（测试音频目录）", code="usage")
            return EXIT_USAGE
        push_audio()
        if not args.skip_login:
            if not args.account_id:
                emit("error", msg="需要 --account-id（或 --skip-login）", code="usage")
                return EXIT_USAGE
            ensure_account(args.account_id, force=args.force_login)
        if args.case:
            from .case_setup import spec_for
            spec = spec_for(args.case)
            go_home()
            close_room()                 # 先保证没有残留房间
            open_room()
            ensure_library(spec["library"])
            if spec.get("volume") is not None:
                ensure_volume(spec["volume"])
            if not spec["room"]:
                close_room()
            emit("done", phase="setup", case=args.case, library=spec["library"], room=bool(spec["room"]))
            return 0
        open_room()
        reset_music_library()
        if not args.keep_room:
            close_room()
        emit("done", phase="setup")
        return 0
    except SystemExit:
        raise
    except Exception as e:  # noqa: BLE001  任何没预料到的错误都要作为事件报出来，且不能泄露凭证
        emit("error", msg=f"{type(e).__name__}: {e}", code="exception")
        return EXIT_STEP


if __name__ == "__main__":
    sys.exit(main())
