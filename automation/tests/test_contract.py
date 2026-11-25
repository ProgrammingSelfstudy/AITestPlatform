"""setup 脚本的契约测试：凭证永不泄露、退出码、事件格式。用假设备，不需要真机。"""
import io
import json

import pytest

from hafa_automation import case_setup, setup_env
from hafa_automation.setup_env import EXIT_CREDS, EXIT_HUMAN, EXIT_STEP, EXIT_USAGE
from tests.conftest import FakeDev, el

USER, PWD = "tester@example.com", "S3cretPassw0rd!"
HOME = [el("Recommend", (0, 200, 200, 260)), el("Message / Message", (0, 1500, 180, 1600))]


def events(capsys) -> list[dict]:
    out = capsys.readouterr().out
    lines = [l for l in out.splitlines() if l.strip()]
    return [json.loads(l) for l in lines]   # 每一行都必须是合法 JSON


def login_pages(typed_into_field: bool = False):
    """欢迎页 -> Email 页 -> 首页。输入框的 content-desc 就是它当前的内容（Flutter 的特点），所以失败时页面文字里会带着输入的值。"""
    def email_page(dev):
        fields = [el("Enter your email and password", (0, 100, 720, 160), False)]
        fields += [el(t, (40, 480, 680, 560)) for t in dev.typed]   # 输入框内容出现在页面文字里
        fields.append(el("Login", (40, 800, 680, 900)))
        return fields

    pages = {"welcome": [el("Welcome to Hafa", (0, 100, 720, 160), False), el("Switch to release environment", (0, 600, 720, 660)),
                         el("Email Login", (40, 400, 680, 480))],
             "home": HOME}

    class Dev(FakeDev):
        def elements(self):
            return email_page(self) if self.cur == "email" else list(self.pages[self.cur])

    def on_tap(cur, x, y, dev):
        if cur == "welcome" and 400 <= y <= 480:
            return "email"
        if cur == "email" and 800 <= y <= 900:
            return "home" if dev.typed == [USER, PWD] and not typed_into_field else "email"
        return cur

    return Dev(pages, "welcome", on_tap)


# ---------------------------------------------------------------- 登录与凭证

def test_登录把凭证打给设备_但绝不出现在输出里(install, capsys):
    dev = install(login_pages())
    setup_env.CREDS = {"username": USER, "password": PWD}
    setup_env.login()
    assert dev.typed == [USER, PWD], "账号密码必须真的输入到设备"
    ev = events(capsys)
    out = json.dumps(ev, ensure_ascii=False)
    assert USER not in out and PWD not in out, f"凭证泄露到了输出里：{out}"
    assert any(e["type"] == "step" and e["ok"] and "登录成功" in e["msg"] for e in ev)


def test_登录失败时错误事件带当前页面但凭证已脱敏(install, capsys):
    dev = install(login_pages(typed_into_field=True))   # 点了登录也进不了首页
    setup_env.CREDS = {"username": USER, "password": PWD}
    with pytest.raises(SystemExit) as e:
        setup_env.login()
    assert e.value.code == EXIT_STEP
    ev = events(capsys)
    err = ev[-1]
    assert err["type"] == "error" and err["code"] == "step_failed"
    # 这一页的元素文字里确实带着输入的凭证（输入框 content-desc = 内容），但输出里必须已脱敏
    assert any(USER in t or PWD in t for t in [el(x).description for x in dev.typed]), "测试前提：页面上确实有凭证"
    out = json.dumps(ev, ensure_ascii=False)
    assert USER not in out and PWD not in out, f"错误事件里泄露了凭证：{out}"
    assert "{{account.password}}" in out and "{{account.username}}" in out, "页面里的凭证应被替换成占位符"


def test_需要登录却没有凭证_退出码4(install, capsys):
    install(login_pages())
    with pytest.raises(SystemExit) as e:
        setup_env.login()
    assert e.value.code == EXIT_CREDS
    assert events(capsys)[-1]["code"] == "need_credentials"


def test_脱敏优先替换密码且忽略太短的值(capsys):
    setup_env.CREDS = {"username": "ab", "password": PWD}
    setup_env.emit("step", ok=True, msg=f"输入 {PWD} 和 ab")
    out = events(capsys)[0]["msg"]
    assert PWD not in out and "{{account.password}}" in out
    assert "和 ab" in out, "2 个字符的账号太短，替换会误伤正常文字，不应替换"


def test_stdin不是合法json_退出码2(monkeypatch, capsys):
    monkeypatch.setattr("sys.stdin", io.StringIO("这不是 JSON"))
    rc = setup_env.main(["--credentials-stdin", "--teardown"])
    assert rc == EXIT_USAGE
    assert events(capsys)[-1]["code"] == "usage"


def test_凭证从stdin读且只在内存(monkeypatch, capsys):
    monkeypatch.setattr("sys.stdin", io.StringIO(json.dumps({"username": USER, "password": PWD})))
    monkeypatch.setattr(setup_env, "ensure_awake", lambda: None)
    monkeypatch.setattr(setup_env, "close_room", lambda: None)
    assert setup_env.main(["--credentials-stdin", "--teardown"]) == 0
    assert setup_env.CREDS == {"username": USER, "password": PWD}
    assert USER not in capsys.readouterr().out


# ---------------------------------------------------------------- 屏幕锁定

def test_屏幕解不开_退出码3而不是假装成功(monkeypatch, capsys):
    calls = []
    monkeypatch.setattr(setup_env, "adb", lambda *a: (calls.append(a), "isKeyguardShowing=true mWakefulness=Awake")[1])
    with pytest.raises(SystemExit) as e:
        setup_env.ensure_awake()
    assert e.value.code == EXIT_HUMAN
    ev = events(capsys)
    assert ev[-1]["code"] == "screen_locked"
    assert not any(x.get("ok") is True for x in ev), "没解锁就不能有「已唤醒并解锁」这种成功事件"
    assert any("wm" in a and "dismiss-keyguard" in a for a in calls), "要先尝试解锁再放弃"


def test_屏幕已解锁则不做多余操作(monkeypatch, capsys):
    calls = []
    monkeypatch.setattr(setup_env, "adb", lambda *a: (calls.append(a), "mWakefulness=Awake isKeyguardShowing=false")[1])
    setup_env.ensure_awake()
    assert not any("KEYCODE_WAKEUP" in a for a in calls), "已经亮着且没锁屏，不需要唤醒"
    assert events(capsys)[-1]["ok"] is True


# ---------------------------------------------------------------- 用法与异常

def test_没有account_id也没有skip_login_退出码2(monkeypatch, capsys):
    monkeypatch.setattr(setup_env, "ensure_awake", lambda: None)
    monkeypatch.setattr(setup_env, "push_audio", lambda: None)
    assert setup_env.main(["--case", "TC-01"]) == EXIT_USAGE
    assert events(capsys)[-1]["code"] == "usage"


def test_未预料的异常也作为事件报出且脱敏(monkeypatch, capsys):
    setup_env.CREDS = {"username": USER, "password": PWD}
    monkeypatch.setattr(setup_env, "ensure_awake", lambda: (_ for _ in ()).throw(LookupError(f"页面上找不到 {PWD}")))
    assert setup_env.main(["--teardown"]) == EXIT_STEP
    last = events(capsys)[-1]
    assert last["type"] == "error" and last["code"] == "exception"
    assert PWD not in last["msg"]


def test_每一行输出都是合法json(install, capsys):
    install(login_pages())
    setup_env.CREDS = {"username": USER, "password": PWD}
    setup_env.login()
    for line in capsys.readouterr().out.splitlines():
        json.loads(line)


# ---------------------------------------------------------------- 登录态缓存

def test_同一账号已在首页就跳过重新登录(install, capsys):
    dev = install(FakeDev({"home": HOME}, "home"))
    setup_env._write_state(account="lease-1")
    setup_env.ensure_account("lease-1")
    assert any("跳过重新登录" in e.get("msg", "") for e in events(capsys))
    assert dev.typed == [], "没有重新登录就不能输入任何文字"


def test_换账号要重新登录并记住新账号(install, capsys, monkeypatch):
    dev = install(login_pages())
    setup_env.CREDS = {"username": USER, "password": PWD}
    # 当前在欢迎页（未登录），缓存里记着别的账号
    setup_env._write_state(account="lease-OLD")
    monkeypatch.setattr(setup_env, "go_home", lambda: None)
    setup_env.ensure_account("lease-NEW")
    assert dev.typed == [USER, PWD]
    assert setup_env._read_state()["account"] == "lease-NEW"


def test_登录失败后缓存不会误记账号(install, monkeypatch):
    install(login_pages(typed_into_field=True))
    setup_env.CREDS = {"username": USER, "password": PWD}
    setup_env._write_state(account="lease-OLD")
    monkeypatch.setattr(setup_env, "go_home", lambda: None)
    with pytest.raises(SystemExit):
        setup_env.ensure_account("lease-NEW")
    assert setup_env._read_state().get("account") is None, "登录失败后缓存必须是「未知」，否则下次会以为已登录"


def test_登录态缓存按设备分开():
    assert "TESTSERIAL" in setup_env.STATE_FILE.name


# ---------------------------------------------------------------- 用例规格

def test_用例规格():
    assert case_setup.spec_for("TC-04") == {"library": [], "room": True}
    assert case_setup.spec_for("TC-08")["volume"] == 50
    assert case_setup.spec_for("TC-10")["library"] == ["tst_normal_del", "tst_normal_01"]
    assert case_setup.spec_for("TC-不存在") == case_setup.DEFAULT
    assert case_setup.has_spec("TC-04") and not case_setup.has_spec("GEN-abc123-1")


def test_新模块用例走通用准备_不要求音频也不开语聊房(install, monkeypatch, capsys):
    dev = install(FakeDev({"home": HOME}, "home"))
    monkeypatch.setattr(setup_env, "ensure_awake", lambda: None)
    monkeypatch.setattr(setup_env, "AUDIO_DIR", None)
    called = []
    monkeypatch.setattr(setup_env, "push_audio", lambda: called.append("push_audio"))
    monkeypatch.setattr(setup_env, "open_room", lambda: called.append("open_room"))
    rc = setup_env.main(["--case", "GEN-abc123-1", "--skip-login"])
    assert rc == 0
    assert called == [], f"通用新模块用例不应调用 Hafa 语聊房逻辑：{called}"
    ev = events(capsys)
    assert ev[-1] == {"type": "done", "phase": "setup", "case": "GEN-abc123-1", "generic": True}
    assert dev.launched, "通用准备应启动被测应用"
