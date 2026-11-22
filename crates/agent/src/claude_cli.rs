//! 经 `claude` 命令行调用 Claude 会员（非交互、流式输出）。
//!
//! 为什么走命令行：Rust 没有官方的 Claude Agent SDK，会员登录只在命令行 / SDK 里可用。
//! 技术验证用的是 SDK，命令行这条路在这里单独验证（见 docs/落地记录.md）。
//!
//! 行为约定：
//! - 提示词走 stdin，不进命令行参数（避免在进程列表里泄露内容）；
//! - 启动前剔除 `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_BASE_URL` / `OPENAI_*`，否则会绕过会员登录；
//! - 逐行解析 `stream-json`；每一行都当成「还活着」的信号，超过 `stall_timeout` 没有任何输出就杀进程并报卡住；
//! - 额度用完（命令行报错文案，或 `rate_limit_event` 状态不是 allowed）返回 [`Failure::Quota`]，**不重试、不换计费方式**；
//! - 账号凭证、令牌不进日志。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::Instant;

/// 需要剔除的环境变量：名字完全匹配，或以 `OPENAI_` 开头。
const STRIPPED_EXACT: [&str; 3] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"];
const STRIPPED_PREFIX: &str = "OPENAI_";

/// 给定当前环境变量名，返回应当从子进程里剔除的那些。独立成纯函数，方便测试。
pub fn env_to_strip<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    names
        .into_iter()
        .filter(|n| STRIPPED_EXACT.contains(n) || n.starts_with(STRIPPED_PREFIX))
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone)]
pub struct ClaudeOpts {
    /// 命令行可执行文件，默认 `claude`
    pub bin: String,
    /// 放在 `bin` 后面、其它参数前面的参数。包装式调用（如 `node cli.js`）用；
    /// 测试里用 `/bin/sh` + 脚本路径来模拟命令行，避免执行新建的可执行文件被系统扫描而变慢
    pub pre_args: Vec<String>,
    /// 模型名来自角色配置，代码里不写死；`None` 用命令行默认
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    /// 只加载这个 MCP 配置里的服务（用于设备 MCP 服务），并关闭其它 MCP
    pub mcp_config: Option<PathBuf>,
    /// 允许使用的内置工具；空表示一个都不给（纯文本判定用）
    pub tools: Vec<String>,
    /// 免确认可用的工具（如 `mcp__device__observe`）。非交互模式下没有人点确认，不在这里的 MCP 工具会被拒绝
    pub allowed_tools: Vec<String>,
    pub effort: Option<String>,
    pub cwd: Option<PathBuf>,
    /// 超过这么久没有任何输出判卡住
    pub stall_timeout: Duration,
    /// 整次调用的总时限
    pub total_timeout: Duration,
}

impl Default for ClaudeOpts {
    fn default() -> Self {
        Self {
            bin: "claude".into(),
            pre_args: vec![],
            model: None,
            system_prompt: None,
            mcp_config: None,
            tools: vec![],
            allowed_tools: vec![],
            effort: None,
            cwd: None,
            stall_timeout: Duration::from_secs(120),
            total_timeout: Duration::from_secs(900),
        }
    }
}

/// 额度窗口信息（来自 `rate_limit_event`）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RateLimit {
    /// `allowed` / `allowed_warning` / 其它（被限流）
    pub status: String,
    /// 例如 `seven_day`、`five_hour`
    pub window: Option<String>,
    pub utilization: Option<f64>,
    /// 重置时间（Unix 秒）
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// 额度用完或被限流：暂停并告知用户，不自动换计费方式
    Quota(String),
    /// 登录失效
    Auth(String),
    /// 太久没有任何输出，进程已被杀掉
    Stalled { secs: u64 },
    /// 超过总时限，进程已被杀掉
    Timeout { secs: u64 },
    /// 起不了进程（命令行不存在等）
    Spawn(String),
    /// 进程异常退出且没有给出结果，附原始 stderr
    Exit { code: Option<i32>, stderr: String },
    /// 命令行给出了错误结果
    Result { subtype: String, message: String },
    /// 正常结束但没有 result 事件
    NoResult,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub text: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    /// 折算费用（USD）。会员不是按它扣费，只作用量参考
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u32>,
    pub duration_ms: Option<u64>,
    /// 命令行报告的各额度窗口；同一次调用可能有多条，这里保留最后一条
    pub rate_limit: Option<RateLimit>,
    /// 模型实际发起的工具调用，按顺序（P1 数步数、验证「真的调了工具」都靠它）
    pub tool_calls: Vec<String>,
    pub failure: Option<Failure>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.failure.is_none()
    }
}

// ---------------------------------------------------------------- 错误分类

fn has_number_429(s: &str) -> bool {
    let b = s.as_bytes();
    s.match_indices("429").any(|(i, _)| {
        let before = i == 0 || !b[i - 1].is_ascii_digit();
        let after = i + 3 >= b.len() || !b[i + 3].is_ascii_digit();
        before && after
    })
}

/// 错误文案是不是额度 / 限流。规则来自技术验证里的识别规则。
pub fn looks_like_quota(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "usage limit",
        "rate limit",
        "rate-limit",
        "ratelimit",
        "quota",
        "too many requests",
        "insufficient credit",
        "insufficient balance",
        "额度",
        "限流",
    ]
    .iter()
    .any(|k| m.contains(k))
        || has_number_429(&m)
        || hit_your_limit(&m)
}

/// 「you've hit / reached your [weekly / daily …] limit」：`your` 和 `limit` 之间最多夹一个词。
fn hit_your_limit(m: &str) -> bool {
    ["hit your ", "reached your "].iter().any(|k| {
        m.match_indices(k).any(|(i, _)| {
            let rest = &m[i + k.len()..];
            let mut words = rest.split_whitespace();
            let first = words.next().unwrap_or("");
            let second = words.next().unwrap_or("");
            first.starts_with("limit") || second.starts_with("limit")
        })
    })
}

fn looks_like_auth(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["not logged in", "please run /login", "invalid api key", "authentication", "unauthorized", "credential", "login"]
        .iter()
        .any(|k| m.contains(k))
}

fn classify_error(subtype: &str, message: &str, api_status: Option<&Value>) -> Failure {
    let blob = format!("{message} {}", api_status.map(|v| v.to_string()).unwrap_or_default());
    if looks_like_quota(&blob) {
        Failure::Quota(message.to_string())
    } else if looks_like_auth(&blob) {
        Failure::Auth(message.to_string())
    } else {
        Failure::Result { subtype: subtype.to_string(), message: message.to_string() }
    }
}

fn parse_rate_limit(info: &Value) -> Option<RateLimit> {
    let status = info.get("status")?.as_str()?.to_string();
    Some(RateLimit {
        status,
        window: info.get("rateLimitType").and_then(Value::as_str).map(str::to_string),
        utilization: info.get("utilization").and_then(Value::as_f64),
        resets_at: info.get("resetsAt").and_then(Value::as_i64),
    })
}

// ---------------------------------------------------------------- 运行

/// 把以 `child` 为组长的整个进程组杀掉（包括它派生的孙进程）。组已不存在时无副作用。
fn kill_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: 只发送信号，不涉及内存；pid 来自刚启动的子进程，且它是自己进程组的组长
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

fn build_args(o: &ClaudeOpts) -> Vec<String> {
    let mut a: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose", "--no-session-persistence"]
        .map(String::from)
        .into();
    // 不加载用户 / 项目的设置，避免本机的钩子、插件影响执行；会员登录不受影响
    a.extend(["--setting-sources".into(), String::new()]);
    // `--tools ""` = 一个内置工具都不给；有列表时按列表给
    a.extend(["--tools".into(), o.tools.join(",")]);
    if let Some(m) = &o.model {
        a.extend(["--model".into(), m.clone()]);
    }
    if let Some(s) = &o.system_prompt {
        a.extend(["--system-prompt".into(), s.clone()]);
    }
    if let Some(e) = &o.effort {
        a.extend(["--effort".into(), e.clone()]);
    }
    if !o.allowed_tools.is_empty() {
        a.extend(["--allowed-tools".into(), o.allowed_tools.join(",")]);
    }
    if let Some(p) = &o.mcp_config {
        a.extend(["--mcp-config".into(), p.to_string_lossy().into_owned(), "--strict-mcp-config".into()]);
    }
    a
}

/// 调用一次。提示词走 stdin。出现的任何问题都放进 `Outcome.failure`，不 panic、不吞错。
pub async fn run(opts: &ClaudeOpts, prompt: &str) -> Outcome {
    run_inner(opts, prompt, false).await
}

/// 纯文本角色隔离外部工具及当前进程的应用凭证，保留本机登录和运行环境。
pub(crate) async fn run_text_only(opts: &ClaudeOpts, prompt: &str) -> Outcome {
    run_inner(opts, prompt, true).await
}

async fn run_inner(opts: &ClaudeOpts, prompt: &str, text_only: bool) -> Outcome {
    let mut out = Outcome::default();

    let mut cmd = Command::new(&opts.bin);
    cmd.args(&opts.pre_args)
        .args(build_args(opts))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true) // 调用方放弃等待时，直接子进程不能留成孤儿
        .process_group(0); // 独立进程组：命令行会拉起 MCP 服务等子进程，卡住时要整组杀，否则它们抓着管道不放
    if let Some(d) = &opts.cwd {
        cmd.current_dir(d);
    }
    if text_only {
        const KEEP: &[&str] = &[
            "HOME", "PATH", "SHELL", "TMPDIR", "TEMP", "TMP", "LANG", "LC_ALL", "LC_CTYPE",
            "USER", "LOGNAME", "CLAUDE_CONFIG_DIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME",
            "XDG_DATA_HOME", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
            "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY",
            "https_proxy", "http_proxy", "all_proxy", "no_proxy",
        ];
        cmd.args(["--mcp-config", r#"{"mcpServers":{}}"#, "--strict-mcp-config"])
            .env_clear()
            .envs(std::env::vars_os().filter(|(name, _)| name.to_str().is_some_and(|n| KEEP.contains(&n))));
    }
    let names: Vec<String> = std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).collect();
    for n in env_to_strip(names.iter().map(String::as_str)) {
        cmd.env_remove(n);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            out.failure = Some(Failure::Spawn(format!("{}：{e}", opts.bin)));
            return out;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        // 写完立刻关闭，命令行才会开始处理
        if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
            tracing::warn!("写入提示词失败：{e}");
        }
        drop(stdin);
    }

    // stderr 单独收，最多留 8KB，失败时原样带出。
    // 读到的内容实时放进共享缓冲：即使收尾时等不到 EOF（残留的子孙进程握着管道），已读到的原始错误也不会丢
    let mut stderr = child.stderr.take().expect("已配置 piped");
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let stderr_task = {
        let buf = stderr_buf.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 1024];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // 超出 8KB 的部分继续读走（不存），避免子进程因管道写满而阻塞
                        let mut b = buf.lock().unwrap();
                        let room = 8192usize.saturating_sub(b.len());
                        b.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        })
    };

    let stdout = child.stdout.take().expect("已配置 piped");
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + opts.total_timeout;
    let mut got_result = false;
    let mut quota_signal: Option<String> = None;

    loop {
        let next = tokio::time::timeout(opts.stall_timeout, lines.next_line());
        let line = match tokio::time::timeout_at(deadline, next).await {
            Err(_) => {
                kill_group(&child);
                let _ = child.kill().await;
                out.failure = Some(Failure::Timeout { secs: opts.total_timeout.as_secs() });
                break;
            }
            Ok(Err(_)) => {
                kill_group(&child);
                let _ = child.kill().await;
                out.failure = Some(Failure::Stalled { secs: opts.stall_timeout.as_secs() });
                break;
            }
            Ok(Ok(Ok(Some(l)))) => l,
            Ok(Ok(Ok(None))) => break, // stdout 关闭
            Ok(Ok(Err(e))) => {
                tracing::warn!("读取输出失败：{e}");
                break;
            }
        };
        // 不是 JSON 的行（警告、提示）忽略，但已经算作「还活着」
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v.get("type").and_then(Value::as_str) {
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
                out.session_id = v.get("session_id").and_then(Value::as_str).map(str::to_string);
                out.model = v.get("model").and_then(Value::as_str).map(str::to_string);
            }
            Some("assistant") => {
                let blocks = v.pointer("/message/content").and_then(Value::as_array);
                for b in blocks.into_iter().flatten() {
                    if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                        if let Some(n) = b.get("name").and_then(Value::as_str) {
                            out.tool_calls.push(n.to_string());
                        }
                    }
                }
            }
            Some("rate_limit_event") => {
                if let Some(rl) = v.get("rate_limit_info").and_then(parse_rate_limit) {
                    // 状态不是 allowed / allowed_warning，说明已经被限流
                    if !rl.status.starts_with("allowed") {
                        quota_signal = Some(format!("额度状态 {}（窗口 {:?}）", rl.status, rl.window));
                    }
                    out.rate_limit = Some(rl);
                }
            }
            Some("result") => {
                got_result = true;
                out.text = v.get("result").and_then(Value::as_str).unwrap_or_default().to_string();
                out.cost_usd = v.get("total_cost_usd").and_then(Value::as_f64);
                out.num_turns = v.get("num_turns").and_then(Value::as_u64).map(|n| n as u32);
                out.duration_ms = v.get("duration_ms").and_then(Value::as_u64);
                if out.session_id.is_none() {
                    out.session_id = v.get("session_id").and_then(Value::as_str).map(str::to_string);
                }
                if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("error");
                    out.failure = Some(match &quota_signal {
                        Some(s) => Failure::Quota(format!("{s}：{}", out.text)),
                        None => classify_error(subtype, &out.text, v.get("api_error_status")),
                    });
                }
            }
            _ => {}
        }
    }

    // 收尾：没被我们杀掉的话，等进程退出拿退出码
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    // 收尾时无论如何清掉残留的子孙进程；等 stderr 最多 2 秒，不能被残留进程无限拖住
    kill_group(&child);
    let _ = tokio::time::timeout(Duration::from_secs(2), stderr_task).await;
    let stderr_text = String::from_utf8_lossy(&stderr_buf.lock().unwrap()).trim().to_string();
    if out.failure.is_none() {
        let code = match status {
            Ok(Ok(s)) => s.code(),
            _ => {
                let _ = child.kill().await;
                None
            }
        };
        if !got_result {
            out.failure = Some(match (&quota_signal, code) {
                (Some(s), _) => Failure::Quota(s.clone()),
                (None, Some(0)) => Failure::NoResult,
                (None, c) if looks_like_quota(&stderr_text) => {
                    let _ = c;
                    Failure::Quota(stderr_text.clone())
                }
                (None, c) => Failure::Exit { code: c, stderr: stderr_text },
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写一个假的 `claude` 脚本，返回它的路径。脚本不会被直接执行，而是交给 `/bin/sh` 去读（见 `opts`）。
    fn fake(script: &str) -> String {
        let dir = std::env::temp_dir().join(format!("fake_claude_{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("claude");
        std::fs::write(&p, format!("{script}\n")).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn uuid_like() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!("{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
    }

    fn opts(script: String) -> ClaudeOpts {
        ClaudeOpts { bin: "/bin/sh".into(), pre_args: vec![script], stall_timeout: Duration::from_secs(5), total_timeout: Duration::from_secs(20), ..Default::default() }
    }

    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"s1","model":"m-test"}"#;
    const WARN: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.83,"resetsAt":1795287600}}"#;

    #[test]
    fn 要剔除的环境变量() {
        let got = env_to_strip(["PATH", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "OPENAI_API_KEY", "OPENAI_ORG_ID", "ANTHROPIC_MODEL", "HOME"]);
        assert_eq!(got, ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "OPENAI_API_KEY", "OPENAI_ORG_ID"]);
    }

    #[test]
    fn 额度文案识别() {
        for s in ["You've hit your usage limit", "Rate limit exceeded", "HTTP 429", "额度已用完", "too many requests", "You have reached your weekly limit"] {
            assert!(looks_like_quota(s), "应识别为额度：{s}");
        }
        for s in ["file not found", "port 14290 busy", "request id 4291", "网络错误"] {
            assert!(!looks_like_quota(s), "不应识别为额度：{s}");
        }
    }

    #[test]
    fn 命令行参数() {
        let o = ClaudeOpts {
            model: Some("m1".into()),
            system_prompt: Some("sp".into()),
            mcp_config: Some("/tmp/mcp.json".into()),
            effort: Some("low".into()),
            ..Default::default()
        };
        let a = build_args(&o);
        for want in ["-p", "stream-json", "--verbose", "--no-session-persistence", "--strict-mcp-config"] {
            assert!(a.iter().any(|x| x == want), "缺少 {want}：{a:?}");
        }
        let at = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].clone());
        assert_eq!(at("--model").as_deref(), Some("m1"));
        assert_eq!(at("--tools").as_deref(), Some(""), "tools 为空要传空串，表示一个内置工具都不给");
        assert_eq!(at("--setting-sources").as_deref(), Some(""));
        assert!(!a.iter().any(|x| x.contains("api") && x.contains("key")), "命令行里不能出现 API Key 相关参数");
    }

    #[tokio::test]
    async fn 正常调用并带回额度信息() {
        let bin = fake(&format!(
            "cat >/dev/null\necho '{INIT}'\necho '{WARN}'\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"OK\",\"total_cost_usd\":0.0186,\"num_turns\":1,\"duration_ms\":2465}}'"
        ));
        let o = run(&opts(bin), "hi").await;
        assert!(o.ok(), "{:?}", o.failure);
        assert_eq!((o.text.as_str(), o.model.as_deref(), o.session_id.as_deref()), ("OK", Some("m-test"), Some("s1")));
        assert_eq!(o.cost_usd, Some(0.0186));
        let rl = o.rate_limit.unwrap();
        assert_eq!((rl.status.as_str(), rl.window.as_deref(), rl.utilization), ("allowed_warning", Some("seven_day"), Some(0.83)));
    }

    #[test]
    fn 允许工具的命令行参数() {
        let o = ClaudeOpts { allowed_tools: vec!["mcp__device__observe".into(), "mcp__device__tap_element".into()], ..Default::default() };
        let a = build_args(&o);
        let i = a.iter().position(|x| x == "--allowed-tools").expect("缺少 --allowed-tools");
        assert_eq!(a[i + 1], "mcp__device__observe,mcp__device__tap_element");
        assert!(!build_args(&ClaudeOpts::default()).iter().any(|x| x == "--allowed-tools"), "没配置时不能传");
    }

    #[tokio::test]
    async fn 记录模型发起的工具调用() {
        let bin = fake(&format!(
            "cat >/dev/null\necho '{INIT}'\necho '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"好\"}},{{\"type\":\"tool_use\",\"name\":\"mcp__device__observe\",\"input\":{{}}}}]}}}}'\necho '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"mcp__device__tap_element\"}}]}}}}'\necho '{{\"type\":\"result\",\"is_error\":false,\"result\":\"完成\"}}'"
        ));
        let o = run(&opts(bin), "x").await;
        assert!(o.ok(), "{:?}", o.failure);
        assert_eq!(o.tool_calls, ["mcp__device__observe", "mcp__device__tap_element"]);
    }

    #[tokio::test]
    async fn 提示词走stdin() {
        let bin = fake(&format!(
            "read line\necho '{INIT}'\nprintf '{{\"type\":\"result\",\"is_error\":false,\"result\":\"got:%s\"}}\\n' \"$line\""
        ));
        let o = run(&opts(bin), "hello").await;
        assert_eq!(o.text, "got:hello");
    }

    #[tokio::test]
    async fn 额度用完报错() {
        let bin = fake(&format!(
            "cat >/dev/null\necho '{INIT}'\necho '{{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"You have hit your usage limit\"}}'"
        ));
        let o = run(&opts(bin), "x").await;
        assert!(matches!(o.failure, Some(Failure::Quota(_))), "{:?}", o.failure);
    }

    #[tokio::test]
    async fn 限流事件被识别为额度() {
        let bin = fake(&format!(
            "cat >/dev/null\necho '{INIT}'\necho '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"rateLimitType\":\"five_hour\"}}}}'\necho '{{\"type\":\"result\",\"is_error\":true,\"result\":\"请求失败\"}}'"
        ));
        let o = run(&opts(bin), "x").await;
        assert!(matches!(o.failure, Some(Failure::Quota(_))), "状态不是 allowed 必须算额度：{:?}", o.failure);
    }

    #[tokio::test]
    async fn 登录失效() {
        let bin = fake(&format!(
            "cat >/dev/null\necho '{INIT}'\necho '{{\"type\":\"result\",\"is_error\":true,\"result\":\"Not logged in · Please run /login\"}}'"
        ));
        let o = run(&opts(bin), "x").await;
        assert!(matches!(o.failure, Some(Failure::Auth(_))), "{:?}", o.failure);
    }

    #[tokio::test]
    async fn 卡住会被检测并杀掉() {
        let bin = fake(&format!("cat >/dev/null\necho '{INIT}'\nsleep 30"));
        let mut op = opts(bin);
        op.stall_timeout = Duration::from_secs(4);
        let t = std::time::Instant::now();
        let o = run(&op, "x").await;
        assert!(matches!(o.failure, Some(Failure::Stalled { .. })), "{:?}", o.failure);
        // 必须是「先收到 init、之后才卡住」，而不是启动太慢被误判成卡住
        assert_eq!(o.session_id.as_deref(), Some("s1"), "卡住之前应已收到 init 事件");
        // 脚本里 `sleep 30`：如果只杀了 sh、没杀它的子进程，这里会被残留进程拖到 30 秒
        assert!(t.elapsed() < Duration::from_secs(15), "卡住后应很快返回（整组杀掉），实际 {:?}", t.elapsed());
    }

    #[tokio::test]
    async fn 持续有输出但超过总时限() {
        let bin = fake("cat >/dev/null\nwhile true; do echo '{\"type\":\"assistant\"}'; sleep 0.1; done");
        let mut o = opts(bin);
        o.total_timeout = Duration::from_secs(2);
        o.stall_timeout = Duration::from_secs(10);
        let r = run(&o, "x").await;
        assert!(matches!(r.failure, Some(Failure::Timeout { .. })), "{:?}", r.failure);
    }

    #[tokio::test]
    async fn 异常退出带出原始stderr() {
        let bin = fake("cat >/dev/null\necho '这是原始错误输出' >&2\nexit 7");
        let o = run(&opts(bin), "x").await;
        match o.failure {
            Some(Failure::Exit { code, stderr }) => {
                assert_eq!(code, Some(7));
                assert!(stderr.contains("这是原始错误输出"), "stderr 必须原样保留：{stderr}");
            }
            other => panic!("期望 Exit，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn 正常结束但没有result() {
        let bin = fake(&format!("cat >/dev/null\necho '{INIT}'"));
        assert_eq!(run(&opts(bin), "x").await.failure, Some(Failure::NoResult));
    }

    #[tokio::test]
    async fn 命令行不存在() {
        let mut op = opts(String::new());
        op.bin = "/不存在/claude".into();
        op.pre_args = vec![];
        let o = run(&op, "x").await;
        assert!(matches!(o.failure, Some(Failure::Spawn(_))), "{:?}", o.failure);
    }

    #[tokio::test]
    async fn 非json行被忽略() {
        let bin = fake(&format!(
            "cat >/dev/null\necho '这不是 JSON'\necho '{INIT}'\necho '{{坏的'\necho '{{\"type\":\"result\",\"is_error\":false,\"result\":\"OK\"}}'"
        ));
        let o = run(&opts(bin), "x").await;
        assert!(o.ok() && o.text == "OK", "{:?}", o.failure);
    }

    /// 真实调用，消耗一点会员额度：`cargo test -p agent -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn 真实调用_会员路径() {
        let mut o = ClaudeOpts::default();
        o.stall_timeout = Duration::from_secs(90);
        let r = run(&o, "只回复两个字母：OK").await;
        println!("结果：{:?}\n模型：{:?}\n折算费用：{:?}\n额度：{:?}", r.text, r.model, r.cost_usd, r.rate_limit);
        assert!(r.ok(), "{:?}", r.failure);
        assert!(r.text.contains("OK"));
    }
}
