//! `agy` 运行器测试。`agy` 用 `/bin/sh` 脚本模拟：不连 Google、不花额度、不碰真实登录。
//! 真实命令行的行为（事件格式、权限规则格式、认证方式）来自 docs/落地记录.md「agy 能不能接证据工具」里的实测，
//! 这里的假事件照着真实事件的字段写。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent::agy_cli::{AgyOpts, Failure, McpServer, check_login, env_to_strip, run};
use serde_json::{Value, json};

struct Fake {
    dir: PathBuf,
    real_home: PathBuf,
    opts: AgyOpts,
}

impl Fake {
    /// `body` 是脚本正文；脚本里的 `$0` 是脚本路径，用 `$0.xxx` 往旁边写文件让测试检查。
    fn new(body: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("hafa-agytest-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let real_home = dir.join("real_home");
        fs::create_dir_all(real_home.join(".gemini/antigravity-cli")).unwrap();
        fs::write(real_home.join(".gemini/antigravity-cli/marker"), "真实的状态目录").unwrap();
        fs::create_dir_all(real_home.join("Library/Keychains")).unwrap();
        fs::write(real_home.join("Library/Keychains/marker"), "真实的钥匙串").unwrap();
        let script = dir.join("agy.sh");
        fs::write(&script, format!("set -u\n{body}\n")).unwrap();
        let opts = AgyOpts {
            bin: "/bin/sh".into(),
            pre_args: vec![script.to_string_lossy().into()],
            model: Some("角色指定的假模型".into()),
            mcp: vec![],
            allowed_tools: vec![],
            real_home: Some(real_home.clone()),
            total_timeout: Duration::from_secs(20),
        };
        Self { dir, real_home, opts }
    }

    fn file(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join(format!("agy.sh.{name}"))).unwrap_or_default()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn evidence() -> McpServer {
    McpServer { name: "evidence".into(), command: "/opt/mcp-evidence".into(), env: BTreeMap::from([("HAFA_RUN_DIR".to_string(), "/runs/a".to_string())]) }
}

fn allow_evidence(opts: &mut AgyOpts) {
    opts.mcp = vec![evidence()];
    opts.allowed_tools = vec![("evidence".into(), "image".into()), ("evidence".into(), "step_text".into())];
}

// ---- 假事件（字段照真实输出写）----

fn init(model: &str) -> Value {
    json!({"event": "init", "init": {"model": model, "cwd": "/x", "tools": ["call_mcp_tool", "read_file"], "permission_mode": "request-review"}})
}

fn tool(index: i64, name: &str, server: Option<(&str, &str)>) -> Value {
    let parameters = server.map_or(json!({"AbsolutePath": "/etc/hosts"}), |(s, t)| json!({"Arguments": {"name": "1_after.png"}, "ServerName": s, "ToolName": t}));
    json!({"event": "step_update", "step_update": {"conversation_id": "conv-1", "step_index": index, "state": "DONE", "step_type": "tool", "tool_name": name, "tool_info": {"name": name, "parameters": parameters}}})
}

fn result(status: &str, response: &str, error: &str, denied: &[&str]) -> Value {
    let mut result = json!({"conversation_id": "conv-1", "status": status, "response": response, "usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 150}});
    if !error.is_empty() {
        result["error"] = json!(error);
    }
    if !denied.is_empty() {
        result["denied_actions"] = json!(denied.iter().map(|action| json!({"action": action})).collect::<Vec<_>>());
    }
    json!({"event": "result", "result": result})
}

/// 把事件写成文件，脚本里 `cat` 出来：避免在 shell 里转义 JSON。
fn emit(fake: &Fake, events: &[Value]) {
    let text: String = events.iter().map(|event| format!("{event}\n")).collect();
    fs::write(fake.dir.join("events.jsonl"), text).unwrap();
}

fn events_script(extra: &str) -> String {
    format!("cat > \"$0.stdin\"\nprintf '%s\\n' \"$@\" > \"$0.args\"\n{extra}\ncat \"$(dirname \"$0\")/events.jsonl\"")
}

#[tokio::test]
async fn 成功_解析文本模型用量和工具调用_提示词只走标准输入() {
    let mut fake = Fake::new(&events_script(""));
    allow_evidence(&mut fake.opts);
    emit(&fake, &[init("假命令行报告的模型"), tool(2, "call_mcp_tool", Some(("evidence", "image"))), tool(3, "call_mcp_tool", Some(("evidence", "step_text"))), result("SUCCESS", "{\"ok\":true}", "", &[])]);
    let outcome = run(&fake.opts, "这是用例内容 SECRET-PROMPT-123").await;
    assert!(outcome.ok(), "{outcome:?}");
    assert_eq!(outcome.text, "{\"ok\":true}");
    assert_eq!(outcome.model.as_deref(), Some("假命令行报告的模型"));
    assert_eq!(outcome.conversation_id.as_deref(), Some("conv-1"));
    assert_eq!(outcome.usage.as_ref().map(|usage| (usage.input_tokens, usage.output_tokens, usage.total_tokens)), Some((100, 20, 150)));
    assert_eq!(outcome.tool_calls.iter().map(|call| call.mcp.clone().unwrap()).collect::<Vec<_>>(), vec![("evidence".to_string(), "image".to_string()), ("evidence".to_string(), "step_text".to_string())]);

    let args = fake.file("args");
    for expected in ["--print=", "--input-format\nstream-json", "--output-format\nstream-json", "--mode\nplan", "--model\n角色指定的假模型", "--settings", "--log-file", "--print-timeout=20s"] {
        assert!(args.contains(expected), "缺少参数 {expected:?}：{args}");
    }
    assert!(!args.contains("SECRET-PROMPT-123"), "提示词不能出现在命令行参数里（进程列表看得到）");
    let stdin: Value = serde_json::from_str(fake.file("stdin").lines().next().unwrap()).unwrap();
    assert_eq!(stdin, json!({"event": "user", "message": {"content": "这是用例内容 SECRET-PROMPT-123"}}));
}

#[tokio::test]
async fn 临时环境_独立_home_空工作目录_专用配置_剔除环境变量_用完清理() {
    // 这些变量会让 agy 改走 API Key / 应用默认凭据：必须被剔除。变量名带前缀，不影响别的测试
    unsafe {
        std::env::set_var("GEMINI_TEST_PROBE_KEY", "不能带进去");
        std::env::set_var("GOOGLE_TEST_PROBE_CRED", "不能带进去");
        std::env::set_var("AGY_TEST_PROBE_AUTH", "不能带进去");
    }
    let mut fake = Fake::new(&events_script(
        r#"echo "$HOME" > "$0.home"; pwd -P > "$0.cwd"; ls -A . > "$0.cwdls"
ls -A "$HOME/.gemini" > "$0.homels"; ls -A "$HOME/Library" > "$0.libls" 2>/dev/null || true
readlink "$HOME/.gemini/antigravity-cli" > "$0.link_state"; readlink "$HOME/Library/Keychains" > "$0.link_keychain" 2>/dev/null || true
cat "$HOME/.gemini/config/mcp_config.json" > "$0.mcp"
while [ $# -gt 0 ]; do case "$1" in --settings) cat "$2" > "$0.settings";; esac; shift; done
env | grep -E '^(GEMINI_|GOOGLE_|AGY_)' > "$0.env" || true"#,
    ));
    allow_evidence(&mut fake.opts);
    emit(&fake, &[init("m"), result("SUCCESS", "好", "", &[])]);
    let outcome = run(&fake.opts, "p").await;
    assert!(outcome.ok(), "{outcome:?}");

    let home = PathBuf::from(fake.file("home").trim());
    assert_ne!(home, fake.real_home, "必须是独立的临时 HOME，不是真实的家目录");
    assert!(home.starts_with(std::env::temp_dir().canonicalize().unwrap()));
    let cwd = PathBuf::from(fake.file("cwd").trim());
    assert_eq!(cwd.parent(), home.parent(), "工作目录和 HOME 在同一个这次专用的临时目录里：{cwd:?} / {home:?}");
    assert_eq!(fake.file("cwdls").trim(), "", "工作目录必须是空的：里面的文件 agy 不用放行就能读");
    assert!(fake.file("homels").contains("antigravity-cli") && fake.file("homels").contains("config"));
    assert_eq!(PathBuf::from(fake.file("link_state").trim()), fake.real_home.join(".gemini/antigravity-cli"), "状态目录是链接，不是复制");
    if cfg!(target_os = "macos") {
        assert_eq!(PathBuf::from(fake.file("link_keychain").trim()), fake.real_home.join("Library/Keychains"), "钥匙串是链接，不是复制");
    }
    let mcp: Value = serde_json::from_str(&fake.file("mcp")).unwrap();
    assert_eq!(mcp, json!({"mcpServers": {"evidence": {"command": "/opt/mcp-evidence", "env": {"HAFA_RUN_DIR": "/runs/a"}, "disabled": false}}}));
    let settings: Value = serde_json::from_str(&fake.file("settings")).unwrap();
    assert_eq!(settings, json!({"permissions": {"allow": ["mcp(evidence/image)", "mcp(evidence/step_text)"]}}), "只放行白名单里的工具");
    assert_eq!(fake.file("env").trim(), "", "GEMINI_ / GOOGLE_ / AGY_ 环境变量必须剔除（规则 1）：{}", fake.file("env"));

    // 用完整个临时目录删除，而链接指向的真实目录原封不动
    assert!(!home.exists(), "临时 HOME 必须清理");
    assert!(!home.parent().unwrap().exists(), "整个临时目录必须清理");
    assert_eq!(fs::read_to_string(fake.real_home.join(".gemini/antigravity-cli/marker")).unwrap(), "真实的状态目录", "清理不能顺着链接删掉真实的登录态");
    assert_eq!(fs::read_to_string(fake.real_home.join("Library/Keychains/marker")).unwrap(), "真实的钥匙串");
}

#[tokio::test]
async fn 审计_白名单之外的任何工具调用都记为违规() {
    let mut fake = Fake::new(&events_script(""));
    allow_evidence(&mut fake.opts);

    // 只调白名单里的：干净
    emit(&fake, &[init("m"), tool(1, "call_mcp_tool", Some(("evidence", "image"))), result("SUCCESS", "x", "", &[])]);
    assert!(run(&fake.opts, "p").await.ok());

    // 内置的读文件（工作目录里的文件不用放行就能读，所以必须靠审计发现）
    emit(&fake, &[init("m"), tool(1, "read_file", None), result("SUCCESS", "x", "", &[])]);
    let outcome = run(&fake.opts, "p").await;
    assert!(!outcome.ok() && outcome.failure.is_none(), "调用本身成功，但结果不可信：{outcome:?}");
    assert_eq!(outcome.violations, vec!["调用了不在白名单内的工具 read_file".to_string()]);

    // 别的 MCP 服务、同一服务里没放行的工具
    emit(&fake, &[init("m"), tool(1, "call_mcp_tool", Some(("other", "image"))), tool(2, "call_mcp_tool", Some(("evidence", "delete_everything"))), result("SUCCESS", "x", "", &[])]);
    let outcome = run(&fake.opts, "p").await;
    assert_eq!(outcome.violations, vec!["调用了不在白名单内的 MCP 工具 other/image".to_string(), "调用了不在白名单内的 MCP 工具 evidence/delete_everything".to_string()]);
    assert_eq!(outcome.tool_calls.len(), 2, "违规的调用也要记下来");

    // 同一步骤的 ACTIVE 和 DONE 两条事件只算一次
    let mut active = tool(1, "read_file", None);
    active["step_update"]["state"] = json!("ACTIVE");
    emit(&fake, &[init("m"), active, tool(1, "read_file", None), result("SUCCESS", "x", "", &[])]);
    assert_eq!(run(&fake.opts, "p").await.violations.len(), 1);

    // 没有放行任何工具时，任何工具调用都是违规
    fake.opts.allowed_tools.clear();
    emit(&fake, &[init("m"), tool(1, "call_mcp_tool", Some(("evidence", "image"))), result("SUCCESS", "x", "", &[])]);
    assert_eq!(run(&fake.opts, "p").await.violations.len(), 1);
}

#[tokio::test]
async fn 被自动拒绝的动作也记下来_结果不可信() {
    let fake = Fake::new(&events_script(""));
    emit(&fake, &[init("m"), result("SUCCESS", "x", "", &["read_file", "command"])]);
    let outcome = run(&fake.opts, "p").await;
    assert_eq!(outcome.denied, vec!["read_file".to_string(), "command".to_string()]);
    assert!(outcome.failure.is_none() && !outcome.ok(), "它想做白名单之外的事，不能采信：{outcome:?}");
}

#[tokio::test]
async fn 失败分类_登录_额度_其他_绝不当成功() {
    let fake = Fake::new(&events_script(""));
    let classify = |status: &str, error: &str| {
        emit(&fake, &[init("m"), result(status, "", error, &[])]);
        run(&fake.opts, "p")
    };
    let auth = classify("ERROR", "authentication failed or timed out").await;
    assert!(matches!(auth.failure, Some(Failure::Auth(ref m)) if m.contains("重新登录") && !m.contains("timed out")), "{auth:?}");
    for text in ["RESOURCE_EXHAUSTED: quota exceeded", "You have reached your usage limit", "429 Too Many Requests", "Gemini Models weekly limit remaining 0%"] {
        let outcome = classify("ERROR", text).await;
        assert!(matches!(outcome.failure, Some(Failure::Quota(_))), "{text} => {outcome:?}");
        assert!(!outcome.ok());
    }
    let other = classify("ERROR", "model produced garbage 秘密账号 someone@example.com").await;
    assert!(matches!(&other.failure, Some(Failure::Result(m)) if !m.contains("someone@example.com") && !m.contains("秘密")), "原始文案可能带账号信息，不能原样往外传：{other:?}");
    // 状态不是 SUCCESS 一律是失败，即使带了响应文本
    emit(&fake, &[init("m"), json!({"event": "result", "result": {"status": "CANCELLED", "response": "看起来像结论"}})]);
    let cancelled = run(&fake.opts, "p").await;
    assert!(cancelled.failure.is_some() && !cancelled.ok(), "{cancelled:?}");
}

#[tokio::test]
async fn 没有结果事件_按退出码和_stderr_分类() {
    let fake = Fake::new(&events_script(""));
    emit(&fake, &[init("m")]);
    assert_eq!(run(&fake.opts, "p").await.failure, Some(Failure::NoResult), "正常退出但没有结果");

    let exit3 = Fake::new("cat > /dev/null; echo '出错了' >&2; exit 3");
    assert_eq!(run(&exit3.opts, "p").await.failure, Some(Failure::Exit { code: Some(3) }));

    let auth = Fake::new("cat > /dev/null; echo \"Error: authentication required. Run 'agy' to log in, then retry.\" >&2; exit 1");
    assert!(matches!(run(&auth.opts, "p").await.failure, Some(Failure::Auth(_))), "stderr 里的登录失败要识别出来，而不是一个笼统的退出码");
}

/// `kill -0`：只探测进程在不在。
fn pid_alive(pid: i32) -> bool {
    std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().is_ok_and(|status| status.success())
}

#[tokio::test]
async fn 超时_杀掉整个进程组_包括子进程_并清理临时目录() {
    // 模拟 agy 起了一个 MCP 子进程：父进程和子进程都在睡
    let mut fake = Fake::new("cat > /dev/null; echo \"$HOME\" > \"$0.home\"\nsleep 60 &\necho $! > \"$0.child\"\necho $$ > \"$0.parent\"\nsleep 60");
    fake.opts.total_timeout = Duration::from_secs(1);
    let started = Instant::now();
    let outcome = run(&fake.opts, "p").await;
    assert_eq!(outcome.failure, Some(Failure::Timeout { secs: 1 }), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (parent, child): (i32, i32) = (fake.file("parent").trim().parse().unwrap(), fake.file("child").trim().parse().unwrap());
    assert!(!pid_alive(parent), "父进程必须被杀掉");
    assert!(!pid_alive(child), "子进程（agy 起的 MCP 服务等）必须一起被杀掉，不能留成孤儿");
    assert!(!PathBuf::from(fake.file("home").trim()).exists(), "超时后临时目录也要清理");
    assert!(!outcome.ok());
}

#[tokio::test]
async fn 起不了进程_如实报告() {
    let fake = Fake::new("");
    let mut opts = fake.opts.clone();
    opts.bin = "/不存在/agy".into();
    opts.pre_args.clear();
    let outcome = run(&opts, "p").await;
    assert!(matches!(outcome.failure, Some(Failure::Spawn(_))), "{outcome:?}");
}

#[tokio::test]
async fn 并发的两次评审各挂各的运行目录_互不串() {
    let fake = Fake::new(&format!("{}\ncp \"$HOME/.gemini/config/mcp_config.json\" \"$0.mcp.$$\"", events_script("")));
    emit(&fake, &[init("m"), result("SUCCESS", "好", "", &[])]);
    let with_dir = |dir: &str| {
        let mut opts = fake.opts.clone();
        opts.mcp = vec![McpServer { name: "evidence".into(), command: "/opt/mcp-evidence".into(), env: BTreeMap::from([("HAFA_RUN_DIR".to_string(), dir.to_string())]) }];
        opts
    };
    let (a, b) = (with_dir("/runs/a"), with_dir("/runs/b"));
    let (first, second) = tokio::join!(run(&a, "p"), run(&b, "p"));
    assert!(first.ok() && second.ok());
    let mut seen: Vec<String> = fs::read_dir(&fake.dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("agy.sh.mcp."))
        .map(|entry| serde_json::from_str::<Value>(&fs::read_to_string(entry.path()).unwrap()).unwrap()["mcpServers"]["evidence"]["env"]["HAFA_RUN_DIR"].as_str().unwrap().to_string())
        .collect();
    seen.sort();
    assert_eq!(seen, vec!["/runs/a".to_string(), "/runs/b".to_string()], "每次评审只看到自己那份配置");
}

#[test]
fn 要剔除的环境变量() {
    let names = ["PATH", "HOME", "GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_APPLICATION_CREDENTIALS", "GOOGLE_CLOUD_PROJECT", "AGY_ADC_AUTH", "ANTHROPIC_API_KEY", "LANG"];
    assert_eq!(env_to_strip(names), vec!["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_APPLICATION_CREDENTIALS", "GOOGLE_CLOUD_PROJECT", "AGY_ADC_AUTH"]);
}

// ---- 登录检查 ----

/// 假的 `agy models`：把一行带认证方式的日志写进 `--log-file` 指定的文件。
fn models_script(log_line: &str, exit: i32) -> String {
    format!("LOG=\nwhile [ $# -gt 0 ]; do case \"$1\" in --log-file) LOG=\"$2\";; esac; shift; done\necho '{log_line}' > \"$LOG\"\necho gemini-4-neon-high\nexit {exit}")
}

#[tokio::test]
async fn 登录检查_只有个人账号登录通过_不调模型() {
    let ok = Fake::new(&models_script("I1122 server_oauth.go:221] applyAuthResult: email=someone@example.com, authMethod=consumer, quotaProject=", 0));
    assert_eq!(check_login(&ok.opts).await, Ok(()));

    for (line, expected) in [
        ("applyAuthResult: email=a@b.c, authMethod=api_key, quotaProject=", "个人账号登录"),
        ("applyAuthResult: email=a@b.c, authMethod=adc, quotaProject=proj", "个人账号登录"),
        ("applyAuthResult: email=a@b.c, authMethod=enterprise, quotaProject=", "个人账号登录"),
        ("没有认证方式这一行", "未检测到"),
    ] {
        let fake = Fake::new(&models_script(line, 0));
        let error = check_login(&fake.opts).await.unwrap_err();
        assert!(error.contains(expected), "{line} => {error}");
        assert!(!error.contains("a@b.c") && !error.contains("proj"), "错误说明里不能带日志里的账号信息：{error}");
    }
    // 命令本身失败（没登录）
    let failed = Fake::new(&models_script("applyAuthResult: authMethod=consumer", 1));
    assert!(check_login(&failed.opts).await.unwrap_err().contains("未检测到"));
}

#[tokio::test]
async fn 登录检查用的也是隔离的环境_并剔除环境变量() {
    unsafe { std::env::set_var("GEMINI_TEST_PROBE_KEY2", "不能带进去") };
    let fake = Fake::new(&format!("echo \"$HOME\" > \"$0.home\"; env | grep -E '^(GEMINI_|GOOGLE_|AGY_)' > \"$0.env\" || true\n{}", models_script("authMethod=consumer", 0)));
    assert_eq!(check_login(&fake.opts).await, Ok(()));
    assert_ne!(PathBuf::from(fake.file("home").trim()), fake.real_home);
    assert_eq!(fake.file("env").trim(), "", "登录检查也不能带着 API Key 类的环境变量：{}", fake.file("env"));
    assert!(!Path::new(fake.file("home").trim()).exists(), "登录检查用的临时目录也要清理");
}
