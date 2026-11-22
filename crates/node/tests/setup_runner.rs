//! Python 自动化脚本运行器的测试：用假脚本（由 /bin/sh 读取）模拟脚本的各种行为和退出码。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use node::setup_runner::{redact, run_setup, AccountArg, Event, SetupResult, SetupSpec};

const USER: &str = "tester@example.com";
const PWD: &str = "S3cretPassw0rd!";

fn tmp() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!("setup_runner_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn account() -> AccountArg {
    AccountArg { id: "lease-1".into(), username: USER.into(), password: PWD.into() }
}

/// 假脚本：写到临时目录，由 /bin/sh 读取。
fn spec(body: &str) -> (SetupSpec, PathBuf) {
    let dir = tmp();
    let script = dir.join("fake.sh");
    std::fs::write(&script, format!("{body}\n")).unwrap();
    let mut s = SetupSpec::new("/bin/sh", &dir, "SERIAL1", "com.hafa.pikka", "com.hafa.pikka.MainActivity");
    s.entry = vec![script.to_string_lossy().into()];
    s.timeout = Duration::from_secs(20);
    (s, script)
}

async fn run(s: &SetupSpec) -> (SetupResult, Vec<Event>) {
    let mut evs = vec![];
    let r = run_setup(s, |e| evs.push(e.clone())).await;
    (r, evs)
}

fn read(script: &PathBuf, suffix: &str) -> String {
    std::fs::read_to_string(format!("{}{suffix}", script.display())).unwrap_or_default()
}

const DONE: &str = r#"echo '{"type":"done","phase":"setup"}'"#;

#[tokio::test]
async fn 正常完成并实时转发事件() {
    let (s, _) = spec(&format!(r#"echo '{{"type":"step","ok":true,"msg":"屏幕已唤醒并解锁"}}'
echo '{{"type":"step","ok":true,"msg":"开房成功"}}'
{DONE}"#));
    let (r, evs) = run(&s).await;
    assert_eq!(r, SetupResult::Ok);
    assert_eq!(evs.iter().map(|e| e.msg.as_str()).collect::<Vec<_>>(), ["屏幕已唤醒并解锁", "开房成功", ""]);
    assert_eq!(evs.last().unwrap().kind, "done");
}

#[tokio::test]
async fn 环境变量只传基础变量和hafa配置() {
    let (mut s, script) = spec(&format!("env > \"$0.env\"\n{DONE}"));
    s.bottom_shift = -84;
    s.audio_dir = "/data/audio".into();
    run(&s).await;
    let env = read(&script, ".env");
    for want in ["HAFA_SERIAL=SERIAL1", "HAFA_APP_PACKAGE=com.hafa.pikka", "HAFA_APP_ACTIVITY=com.hafa.pikka.MainActivity", "HAFA_BOTTOM_SHIFT=-84", "HAFA_AUDIO_DIR=/data/audio"] {
        assert!(env.contains(want), "缺少 {want}：{env}");
    }
    // 测试进程自己一定有 USER / SHELL；子进程里不应有，说明环境被清空后只放行了白名单
    assert!(!env.contains("\nUSER=") && !env.contains("\nSHELL="), "非白名单变量泄露给了脚本：{env}");
}

#[tokio::test]
async fn 凭证只经stdin_不进命令行也不进环境变量() {
    let (mut s, script) = spec(&format!("env > \"$0.env\"\necho \"$@\" > \"$0.args\"\ncat > \"$0.stdin\"\n{DONE}"));
    s.account = Some(account());
    s.case = Some("TC-04".into());
    assert_eq!(run(&s).await.0, SetupResult::Ok);
    let stdin: serde_json::Value = serde_json::from_str(&read(&script, ".stdin")).expect("stdin 应是 JSON");
    assert_eq!((stdin["username"].as_str(), stdin["password"].as_str()), (Some(USER), Some(PWD)), "凭证要经 stdin 传到");
    for (name, content) in [("命令行", read(&script, ".args")), ("环境变量", read(&script, ".env"))] {
        assert!(!content.contains(PWD) && !content.contains(USER), "凭证泄露到了{name}：{content}");
    }
    let args = read(&script, ".args");
    assert!(args.contains("--case TC-04") && args.contains("--account-id lease-1") && args.contains("--credentials-stdin"), "{args}");
}

#[tokio::test]
async fn 脚本万一泄露凭证_rust侧也会脱敏() {
    let (mut s, _) = spec(&format!(
        r#"echo '{{"type":"step","ok":true,"msg":"输入了 {PWD} 和 {USER}"}}'
echo '{{"type":"error","msg":"失败 {PWD}","code":"step_failed","page":["{USER}","{PWD}","Login"]}}'
exit 1"#
    ));
    s.account = Some(account());
    let (r, evs) = run(&s).await;
    let all = format!("{r:?} {evs:?}");
    assert!(!all.contains(PWD) && !all.contains(USER), "凭证泄露了：{all}");
    assert!(all.contains("{{account.password}}") && all.contains("{{account.username}}"));
    match r {
        SetupResult::StepFailed { page, .. } => assert_eq!(page, ["{{account.username}}", "{{account.password}}", "Login"]),
        other => panic!("期望 StepFailed，实际 {other:?}"),
    }
}

#[tokio::test]
async fn 账号的debug输出不含密码() {
    let t = format!("{:?}", account());
    assert!(!t.contains(PWD) && !t.contains(USER), "{t}");
    assert!(t.contains("lease-1"), "租约 id 不是秘密，可以打印");
}

#[tokio::test]
async fn 退出码1_某一步失败带页面现场() {
    let (s, _) = spec(r#"echo '{"type":"error","msg":"步骤失败：开房成功","code":"step_failed","page":["Voice Party","Start Party"]}'
exit 1"#);
    match run(&s).await.0 {
        SetupResult::StepFailed { msg, page } => assert_eq!((msg.as_str(), page), ("步骤失败：开房成功", vec!["Voice Party".into(), "Start Party".into()])),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn 退出码3_需要人工() {
    let (s, _) = spec(r#"echo '{"type":"error","msg":"屏幕仍然锁定","code":"screen_locked"}'
exit 3"#);
    assert_eq!(run(&s).await.0, SetupResult::NeedHuman { reason: "屏幕仍然锁定".into() });
}

#[tokio::test]
async fn 退出码4_需要凭证() {
    let (s, _) = spec(r#"echo '{"type":"error","msg":"需要登录但没有提供账号凭证","code":"need_credentials"}'
exit 4"#);
    assert_eq!(run(&s).await.0, SetupResult::NeedCredentials);
}

#[tokio::test]
async fn 退出码2_用法错误() {
    let (s, _) = spec(r#"echo '{"type":"error","msg":"缺少 HAFA_AUDIO_DIR","code":"usage"}'
exit 2"#);
    assert_eq!(run(&s).await.0, SetupResult::Usage { msg: "缺少 HAFA_AUDIO_DIR".into() });
}

#[tokio::test]
async fn 脚本崩了带出stderr尾部() {
    let (s, _) = spec("echo 'Traceback: 这是原始错误' >&2\nexit 7");
    match run(&s).await.0 {
        SetupResult::Crashed { code, detail } => {
            assert_eq!(code, Some(7));
            assert!(detail.contains("这是原始错误"), "原始错误要原样带出：{detail}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn 退出码0但没有done事件算不符合契约() {
    let (s, _) = spec(r#"echo '{"type":"step","ok":true,"msg":"x"}'"#);
    assert!(matches!(run(&s).await.0, SetupResult::Crashed { code: Some(0), .. }), "没有 done 不能当成功");
}

#[tokio::test]
async fn 有error事件却退出码0_不能当成功() {
    let (s, _) = spec(&format!("echo '{{\"type\":\"error\",\"msg\":\"出错了\"}}'\n{DONE}"));
    assert!(matches!(run(&s).await.0, SetupResult::Crashed { .. }), "既有 error 又退出 0，说明脚本有问题，不能放行");
}

#[tokio::test]
async fn 非json输出行被忽略() {
    let (s, _) = spec(&format!("echo 'urllib3 警告：不是 JSON'\necho '{{坏的'\n{DONE}"));
    let (r, evs) = run(&s).await;
    assert_eq!(r, SetupResult::Ok);
    assert_eq!(evs.len(), 1, "只有合法的 JSON 事件被转发");
}

#[tokio::test]
async fn 超时后整组杀掉而不是被残留进程拖住() {
    let (mut s, _) = spec("sleep 30\n");
    s.timeout = Duration::from_millis(800);
    let t = Instant::now();
    assert_eq!(run(&s).await.0, SetupResult::Timeout { secs: 0 });
    assert!(t.elapsed() < Duration::from_secs(8), "超时后要马上返回（整组杀掉），实际 {:?}", t.elapsed());
}

#[tokio::test]
async fn 命令行参数的组装() {
    let mut s = SetupSpec::new("python", "/a", "S", "p", "a");
    s.case = Some("TC-08".into());
    assert_eq!(s.args(), ["--case", "TC-08", "--skip-login"], "没有账号就跳过登录");
    s.account = Some(account());
    s.keep_room = true;
    s.force_login = true;
    assert_eq!(s.args(), ["--case", "TC-08", "--account-id", "lease-1", "--credentials-stdin", "--keep-room", "--force-login"]);
    s.teardown = true;
    assert_eq!(s.args(), ["--teardown"], "收尾只做关房，不带账号等任何别的参数");
}

#[test]
fn 脱敏函数() {
    let a = account();
    assert_eq!(redact(&format!("{PWD} / {USER}"), Some(&a)), "{{account.password}} / {{account.username}}");
    assert_eq!(redact("没有账号时原样", None), "没有账号时原样");
    let short = AccountArg { id: "x".into(), username: "ab".into(), password: "cd".into() };
    assert_eq!(redact("ab cd", Some(&short)), "ab cd", "太短的值不替换，免得误伤正常文字");
}

#[tokio::test]
async fn 启动不了解释器时明确报错() {
    let mut s = SetupSpec::new("/不存在/python", "/tmp", "S", "p", "a");
    s.entry = vec![];
    match run(&s).await.0 {
        SetupResult::Crashed { detail, .. } => assert!(detail.contains("无法启动"), "{detail}"),
        other => panic!("{other:?}"),
    }
}
