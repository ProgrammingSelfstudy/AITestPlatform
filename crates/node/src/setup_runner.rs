//! 调起 Python 自动化脚本（setup / teardown）。
//!
//! 规划里「Python 只用于脚本执行和自动化这一层」：节点是 Rust，setup / teardown 是自动化脚本，
//! 沿用技术验证里在真机上验证过的实现（`automation/hafa_automation`）。契约见该包的 `setup_env.py` 头部。
//!
//! 凭证规则：账号密码**只经 stdin** 传给脚本（不进命令行、不进环境变量、不落盘），并且这里在转发任何输出之前
//! 再脱敏一次——不能把「脚本不会泄露」当成前提。

use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// 要执行的账号。`Debug` 不会打印密码。
#[derive(Clone)]
pub struct AccountArg {
    /// 租约 id（不透明标识），脚本只用它判断手机上是不是已经登录这个账号
    pub id: String,
    pub username: String,
    pub password: String,
}

impl fmt::Debug for AccountArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountArg").field("id", &self.id).field("username", &"<隐藏>").field("password", &"<隐藏>").finish()
    }
}

#[derive(Debug, Clone)]
pub struct SetupSpec {
    /// Python 解释器
    pub python: String,
    /// 解释器后面、其它参数前面的参数，默认 `-m hafa_automation`；测试里换成假脚本
    pub entry: Vec<String>,
    /// `automation/` 目录（`-m` 要在这里才找得到包）
    pub automation_dir: PathBuf,
    pub serial: String,
    pub package: String,
    pub activity: String,
    pub bottom_shift: i32,
    pub audio_dir: PathBuf,
    pub state_dir: PathBuf,
    /// 用例编号（setup）；`None` 时只做通用 setup
    pub case: Option<String>,
    /// 账号；`None` 表示不处理登录（假定手机上已经是目标账号）
    pub account: Option<AccountArg>,
    pub teardown: bool,
    pub keep_room: bool,
    pub force_login: bool,
    pub timeout: Duration,
    /// 传给脚本的基础环境变量（HOME、PATH 等），其余一概不传
    pub base_env: Vec<(String, String)>,
}

impl SetupSpec {
    pub fn new(python: impl Into<String>, automation_dir: impl Into<PathBuf>, serial: &str, package: &str, activity: &str) -> Self {
        let keep = ["HOME", "PATH", "TMPDIR", "LANG", "LC_ALL"];
        Self {
            python: python.into(),
            entry: vec!["-m".into(), "hafa_automation".into()],
            automation_dir: automation_dir.into(),
            serial: serial.into(),
            package: package.into(),
            activity: activity.into(),
            bottom_shift: 0,
            audio_dir: PathBuf::new(),
            state_dir: std::env::temp_dir(),
            case: None,
            account: None,
            teardown: false,
            keep_room: false,
            force_login: false,
            timeout: Duration::from_secs(600),
            base_env: std::env::vars().filter(|(k, _)| keep.contains(&k.as_str())).collect(),
        }
    }

    /// 命令行参数（不含解释器和 entry）。凭证不在里面。
    pub fn args(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![];
        if self.teardown {
            a.push("--teardown".into());
        } else {
            if let Some(c) = &self.case {
                a.extend(["--case".into(), c.clone()]);
            }
            match &self.account {
                Some(acc) => a.extend(["--account-id".into(), acc.id.clone(), "--credentials-stdin".into()]),
                None => a.push("--skip-login".into()),
            }
            if self.keep_room {
                a.push("--keep-room".into());
            }
            if self.force_login {
                a.push("--force-login".into());
            }
        }
        a
    }

    fn env(&self) -> Vec<(String, String)> {
        let mut e = self.base_env.clone();
        e.extend([
            ("HAFA_SERIAL".into(), self.serial.clone()),
            ("HAFA_APP_PACKAGE".into(), self.package.clone()),
            ("HAFA_APP_ACTIVITY".into(), self.activity.clone()),
            ("HAFA_BOTTOM_SHIFT".into(), self.bottom_shift.to_string()),
            ("HAFA_AUDIO_DIR".into(), self.audio_dir.to_string_lossy().into_owned()),
            ("HAFA_STATE_DIR".into(), self.state_dir.to_string_lossy().into_owned()),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ]);
        e
    }
}

/// 脚本输出的一个事件（已脱敏）。
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: String,
    pub ok: Option<bool>,
    pub msg: String,
    pub code: Option<String>,
    pub page: Vec<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetupResult {
    Ok,
    /// 某一步失败：带当前页面文字，给用户看原始现场
    StepFailed { msg: String, page: Vec<String> },
    /// 需要人工（如屏幕解不开）
    NeedHuman { reason: String },
    /// 需要登录但没有提供凭证
    NeedCredentials,
    /// 用法或环境错误（缺环境变量等）
    Usage { msg: String },
    /// 脚本崩了或输出不符合契约，附退出码和 stderr 尾部
    Crashed { code: Option<i32>, detail: String },
    /// 超时，进程组已被杀掉
    Timeout { secs: u64 },
}

/// 把文字里的账号密码替换成占位符。长的先替换；太短的值不替换，免得误伤正常文字。
pub fn redact(text: &str, acc: Option<&AccountArg>) -> String {
    let Some(a) = acc else { return text.to_string() };
    let mut t = text.to_string();
    for (v, p) in [(&a.password, "{{account.password}}"), (&a.username, "{{account.username}}")] {
        if v.chars().count() >= 3 {
            t = t.replace(v.as_str(), p);
        }
    }
    t
}

fn parse_event(line: &str, acc: Option<&AccountArg>) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;
    let s = |k: &str| obj.get(k).and_then(Value::as_str).map(|x| redact(x, acc));
    Some(Event {
        kind: s("type")?,
        ok: obj.get("ok").and_then(Value::as_bool),
        msg: s("msg").unwrap_or_default(),
        code: s("code"),
        page: obj.get("page").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|x| redact(x, acc)).collect()).unwrap_or_default(),
        raw: serde_json::from_str(&redact(line, acc)).unwrap_or(Value::Null),
    })
}

/// 杀掉整个进程组。
fn kill_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: 只发信号；pid 是自己启动、自己进程组的组长
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

/// 运行一次 setup / teardown。每个事件（已脱敏）实时交给 `on_event`。
pub async fn run_setup(spec: &SetupSpec, mut on_event: impl FnMut(&Event)) -> SetupResult {
    let mut cmd = Command::new(&spec.python);
    cmd.args(&spec.entry)
        .args(spec.args())
        .current_dir(&spec.automation_dir)
        .env_clear()
        .envs(spec.env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return SetupResult::Crashed { code: None, detail: format!("无法启动 {}：{e}", spec.python) },
    };

    // 凭证只经 stdin：写完立刻关闭
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(a) = &spec.account {
            let _ = stdin.write_all(json!({"username": a.username, "password": a.password}).to_string().as_bytes()).await;
        }
    }
    let mut stderr = child.stderr.take().expect("已配置 piped");
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(8192).read_to_end(&mut buf).await;
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        String::from_utf8_lossy(&buf).to_string()
    });

    let acc = spec.account.as_ref();
    let mut lines = BufReader::new(child.stdout.take().expect("已配置 piped")).lines();
    let deadline = tokio::time::Instant::now() + spec.timeout;
    let mut last_error: Option<Event> = None;
    let mut done = false;
    let timed_out = loop {
        match tokio::time::timeout_at(deadline, lines.next_line()).await {
            Err(_) => break true,
            Ok(Ok(Some(l))) => {
                // 不是 JSON 的行（第三方库的警告）忽略
                if let Some(ev) = parse_event(&l, acc) {
                    match ev.kind.as_str() {
                        "done" => done = true,
                        "error" => last_error = Some(ev.clone()),
                        _ => {}
                    }
                    on_event(&ev);
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) => break false,
        }
    };
    if timed_out {
        kill_group(&child);
        let _ = child.wait().await;
        err_task.abort();
        return SetupResult::Timeout { secs: spec.timeout.as_secs() };
    }
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    kill_group(&child); // 清掉残留的子孙进程
    let code = match status {
        Ok(Ok(s)) => s.code(),
        _ => None,
    };
    let stderr_text = tokio::time::timeout(Duration::from_secs(2), err_task).await.ok().and_then(Result::ok).unwrap_or_default();
    let stderr_tail: String = redact(stderr_text.trim(), acc).chars().rev().take(700).collect::<Vec<_>>().into_iter().rev().collect();

    match (code, last_error) {
        (Some(0), None) if done => SetupResult::Ok,
        (Some(0), _) => SetupResult::Crashed { code, detail: "脚本退出码为 0，但没有输出 done 事件（不符合契约）".into() },
        (Some(1), Some(e)) => SetupResult::StepFailed { msg: e.msg, page: e.page },
        (Some(2), Some(e)) => SetupResult::Usage { msg: e.msg },
        (Some(3), Some(e)) => SetupResult::NeedHuman { reason: e.msg },
        (Some(4), _) => SetupResult::NeedCredentials,
        (c, e) => SetupResult::Crashed { code: c, detail: e.map(|e| e.msg).filter(|m| !m.is_empty()).unwrap_or(stderr_tail) },
    }
}
