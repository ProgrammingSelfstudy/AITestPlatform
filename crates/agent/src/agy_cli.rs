//! 经 `agy` 命令行调用 Gemini（非交互、流式输出），给「独立评审员」用。
//!
//! 为什么要单独写一层，而不是照 `claude_cli` 套一遍：`agy` 的行为和 Claude 命令行有几处关键差别，
//! 都是在真机之外用合成证据逐条验证过的（见 docs/落地记录.md「agy 能不能接证据工具」）：
//!
//! 1. **没有「只给这几个工具」的开关。** 它自带 60 个内置工具（读文件、执行命令、浏览器……）。
//!    无人值守时没放行的权限默认拒绝，但**工作目录里的文件不用放行就能读**，而且被默认放行的工具我们看不到。
//!    所以：工作目录固定成一个空的临时目录；放行规则只写证据工具；再靠流式事件**审计**它实际调用了什么，
//!    凡是白名单之外的工具调用都记为违规，由调用方决定怎么处理（评审员的做法是转待人工确认）。
//! 2. **MCP 配置是全局的**（`~/.gemini/config/mcp_config.json`），`--settings` 不接受 `mcpServers`。
//!    并发的多次评审要各挂各的运行目录，所以每次调用用一个独立的临时 `HOME`：里面放这次专用的 MCP 配置，
//!    再把登录态要用的东西链接进来（状态目录 `.gemini/antigravity-cli`，macOS 上还有 `Library/Keychains`，
//!    登录令牌在钥匙串里）。**只链接，不复制令牌。** 这套做法只在 macOS 上验证过。
//! 3. **提示词走 stdin**（`--input-format stream-json`），不进命令行参数，避免用例内容出现在进程列表里。
//!
//! 约定（对应 CLAUDE.md 的规则）：
//! - 规则 1：只用本机个人账号登录，不用 API Key。启动前剔除 `GEMINI_*` / `GOOGLE_*` / `AGY_*` 环境变量，
//!   并在调用前用 [`check_login`] 确认认证方式是个人账号（`consumer`）。
//! - 额度用完返回 [`Failure::Quota`]，**不重试、不换计费方式**；任何看不懂的失败都当失败处理，绝不当成通过。
//! - 不记录账号、令牌；子进程的 stderr 只用来分类，不原样往外传。

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::Instant;

use crate::claude_cli::looks_like_quota;

/// 要从子进程环境里剔除的前缀。`GEMINI_API_KEY` / `GOOGLE_API_KEY` 会让它改走 API Key 计费，
/// `GOOGLE_APPLICATION_CREDENTIALS` / `AGY_ADC_AUTH` 会让它改走应用默认凭据，`GOOGLE_CLOUD_*` 会带出云项目。
const STRIPPED_PREFIXES: [&str; 3] = ["GEMINI_", "GOOGLE_", "AGY_"];

/// 子进程输出的总量上限：评审的结果只有几 KB，超过这么多说明出了问题，不能让它把内存吃光。
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// 给定当前环境变量名，返回应当从子进程里剔除的那些。独立成纯函数，方便测试。
pub fn env_to_strip<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    names.into_iter().filter(|name| STRIPPED_PREFIXES.iter().any(|prefix| name.starts_with(prefix))).map(str::to_string).collect()
}

/// 要挂给 `agy` 的一个 stdio MCP 服务。
#[derive(Debug, Clone)]
pub struct McpServer {
    pub name: String,
    pub command: PathBuf,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct AgyOpts {
    /// 命令行可执行文件，默认 `agy`
    pub bin: String,
    /// 放在 `bin` 后面、其它参数前面的参数。测试里用 `/bin/sh` + 脚本路径来模拟命令行
    pub pre_args: Vec<String>,
    /// 模型名来自角色配置，代码里不写死。`agy` 的模型名里已经带了推理强度（如 `gemini-4-neon-high`）
    pub model: Option<String>,
    /// 这次调用能用的 MCP 服务，写进这次专用的临时配置
    pub mcp: Vec<McpServer>,
    /// 放行的 MCP 工具 `(服务名, 工具名)`。**同时也是审计的白名单**：调用了这里以外的任何工具都记为违规
    pub allowed_tools: Vec<(String, String)>,
    /// 真实的家目录，登录态从这里链接。`None` 用环境变量 `HOME`
    pub real_home: Option<PathBuf>,
    /// 整次调用的总时限。流式输出里没有可靠的「还活着」信号（思考阶段可能很久没有事件），所以只有总时限
    pub total_timeout: Duration,
}

impl Default for AgyOpts {
    fn default() -> Self {
        Self { bin: "agy".into(), pre_args: vec![], model: None, mcp: vec![], allowed_tools: vec![], real_home: None, total_timeout: Duration::from_secs(600) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// 额度用完或被限流：暂停并告知用户，不自动换计费方式
    Quota(String),
    /// 登录失效或认证方式不对
    Auth(String),
    /// 超过总时限，进程组已被杀掉
    Timeout { secs: u64 },
    /// 起不了进程（命令行不存在等）或准备不了临时环境
    Spawn(String),
    /// 进程异常退出且没有给出结果
    Exit { code: Option<i32> },
    /// 命令行给出了错误结果（原始文案可能带本机信息，只保留分类后的简短说明）
    Result(String),
    /// 正常结束但没有 result 事件
    NoResult,
}

/// 模型实际发起的一次工具调用。**只记名字，不记参数**：参数里可能有路径或用例内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// 内置工具名，MCP 调用统一是 `call_mcp_tool`
    pub tool: String,
    /// MCP 调用的服务名和工具名
    pub mcp: Option<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub text: String,
    pub conversation_id: Option<String>,
    /// 命令行报告的实际模型
    pub model: Option<String>,
    pub usage: Option<Usage>,
    /// 实际发起的全部工具调用，按顺序
    pub tool_calls: Vec<ToolCall>,
    /// 白名单之外的工具调用，每条一句话说明。非空说明这次评审不可信
    pub violations: Vec<String>,
    /// 因无人值守不能确认而被自动拒绝的权限（如 `read_file`、`command`）。非空说明它想做白名单之外的事
    pub denied: Vec<String>,
    pub wall_s: f64,
    pub failure: Option<Failure>,
}

impl Outcome {
    /// 调用成功、没有违规、也没有被拒绝的动作。评审员只有在这种情况下才能采信结果。
    pub fn ok(&self) -> bool {
        self.failure.is_none() && self.violations.is_empty() && self.denied.is_empty()
    }
}

// ---------------------------------------------------------------- 临时环境

/// 一次调用用的临时环境：独立 HOME（含专用 MCP 配置）、空的工作目录、会话级设置文件、日志文件。
/// 丢弃时整个目录删除；里面的符号链接只删链接本身，不会动链接指向的真实目录。
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn work(&self) -> PathBuf {
        self.root.join("work")
    }
    fn settings(&self) -> PathBuf {
        self.root.join("settings.json")
    }
    fn log(&self) -> PathBuf {
        self.root.join("agy.log")
    }

    fn create(opts: &AgyOpts) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().canonicalize()?;
        let root = loop {
            let dir = base.join(format!("hafa-agy-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        let sandbox = Self { root };
        // 构造失败时 Drop 会清理已经建好的部分
        sandbox.populate(opts)?;
        Ok(sandbox)
    }

    fn populate(&self, opts: &AgyOpts) -> io::Result<()> {
        let real_home = match &opts.real_home {
            Some(path) => path.clone(),
            None => std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有 HOME 环境变量，找不到 agy 的登录态"))?,
        };
        let (home, work) = (self.home(), self.work());
        fs::create_dir_all(home.join(".gemini/config"))?;
        fs::create_dir(&work)?;
        // 登录态：状态目录和（macOS 上）钥匙串。只链接，不复制令牌。目标不存在就不建链接，后面登录检查会如实报没登录
        let link = |from: PathBuf, to: PathBuf| -> io::Result<()> {
            if from.exists() {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)?;
                }
                std::os::unix::fs::symlink(from, to)?;
            }
            Ok(())
        };
        link(real_home.join(".gemini/antigravity-cli"), home.join(".gemini/antigravity-cli"))?;
        if cfg!(target_os = "macos") {
            link(real_home.join("Library/Keychains"), home.join("Library/Keychains"))?;
        }
        // 这次专用的 MCP 配置
        let servers: serde_json::Map<String, Value> = opts
            .mcp
            .iter()
            .map(|server| {
                let command = server.command.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "MCP 服务的路径必须是 UTF-8"))?;
                Ok((server.name.clone(), json!({"command": command, "env": server.env, "disabled": false})))
            })
            .collect::<io::Result<_>>()?;
        fs::write(home.join(".gemini/config/mcp_config.json"), serde_json::to_vec(&json!({"mcpServers": servers}))?)?;
        // 会话级设置：只放行白名单里的 MCP 工具。其余权限无人值守时默认拒绝
        let allow: Vec<String> = opts.allowed_tools.iter().map(|(server, tool)| format!("mcp({server}/{tool})")).collect();
        fs::write(self.settings(), serde_json::to_vec(&json!({"permissions": {"allow": allow}}))?)?;
        Ok(())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn command(opts: &AgyOpts, sandbox: &Sandbox) -> Command {
    let mut cmd = Command::new(&opts.bin);
    cmd.args(&opts.pre_args).kill_on_drop(true).process_group(0).current_dir(sandbox.work());
    let names: Vec<String> = std::env::vars_os().filter_map(|(key, _)| key.into_string().ok()).collect();
    for name in env_to_strip(names.iter().map(String::as_str)) {
        cmd.env_remove(name);
    }
    cmd.env("HOME", sandbox.home());
    cmd
}

/// 杀掉整个进程组（`agy` 会再起 MCP 服务等子进程）。
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        // 安全性：只对我们自己用 process_group(0) 起的子进程组发信号
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
}

// ---------------------------------------------------------------- 登录检查

/// 调用前确认本机是个人账号登录（规则 1）。不调模型、不花额度：跑 `agy models`，从它的日志里读认证方式。
/// 只有 `consumer`（个人账号）通过；API Key、应用默认凭据、企业网关、读不出来都不通过。
pub async fn check_login(opts: &AgyOpts) -> Result<(), &'static str> {
    let sandbox = Sandbox::create(&AgyOpts { mcp: vec![], allowed_tools: vec![], ..opts.clone() }).map_err(|_| "无法准备 agy 的临时环境")?;
    let mut cmd = command(opts, &sandbox);
    cmd.args(["--log-file"]).arg(sandbox.log()).arg("models").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let status = tokio::time::timeout(Duration::from_secs(30), cmd.status())
        .await
        .map_err(|_| "agy 登录检查超时（30 秒）")?
        .map_err(|_| "无法运行 agy 命令行检查登录")?;
    let log = fs::read_to_string(sandbox.log()).unwrap_or_default();
    // 日志里有账号邮箱，只取认证方式这一个词
    let method = log.split("authMethod=").nth(1).map(|rest| rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').next().unwrap_or("")).unwrap_or("");
    match (status.success(), method) {
        (true, "consumer") => Ok(()),
        (true, "") | (false, _) => Err("未检测到 agy 的个人账号登录，请在本机运行 `agy` 登录"),
        (true, _) => Err("agy 不是个人账号登录（可能是 API Key、应用默认凭据或企业网关），平台只接受个人账号登录，不使用 API Key"),
    }
}

// ---------------------------------------------------------------- 运行

fn classify_error(message: &str) -> Failure {
    let lower = message.to_ascii_lowercase();
    if lower.contains("authentication") || lower.contains("not logged in") || lower.contains("sign in") || lower.contains("unauthenticated") {
        // 原始文案可能带账号信息，只保留固定说明
        Failure::Auth("agy 登录失效或认证失败，请在本机运行 `agy` 重新登录".into())
    } else if looks_like_quota(message) || lower.contains("resource_exhausted") || lower.contains("resource exhausted") || lower.contains("limit remaining") || lower.contains("usage limit") || lower.contains("quota") {
        Failure::Quota("agy 的额度用完或被限流".into())
    } else {
        Failure::Result("agy 命令行返回了错误结果".into())
    }
}

/// 处理一行流式事件，累积到 `outcome`。看不懂的事件忽略（不当成成功，也不当成失败）。
fn absorb(event: &Value, opts: &AgyOpts, outcome: &mut Outcome, seen_steps: &mut std::collections::HashSet<i64>, result_seen: &mut bool) {
    match event["event"].as_str() {
        Some("init") => {
            outcome.model = event["init"]["model"].as_str().map(str::to_string);
        }
        Some("step_update") => {
            let step = &event["step_update"];
            outcome.conversation_id = outcome.conversation_id.take().or_else(|| step["conversation_id"].as_str().map(str::to_string));
            // 工具调用的 ACTIVE 和 DONE 各来一条，按步骤号只记一次
            if step["step_type"] == "tool"
                && let Some(index) = step["step_index"].as_i64()
                && seen_steps.insert(index)
            {
                let tool = step["tool_name"].as_str().unwrap_or("（未知工具）").to_string();
                let mcp = (tool == "call_mcp_tool")
                    .then(|| {
                        let parameters = &step["tool_info"]["parameters"];
                        Some((parameters["ServerName"].as_str()?.to_string(), parameters["ToolName"].as_str()?.to_string()))
                    })
                    .flatten();
                let allowed = mcp.as_ref().is_some_and(|(server, name)| opts.allowed_tools.iter().any(|(s, t)| s == server && t == name));
                if !allowed {
                    outcome.violations.push(match &mcp {
                        Some((server, name)) => format!("调用了不在白名单内的 MCP 工具 {server}/{name}"),
                        None => format!("调用了不在白名单内的工具 {tool}"),
                    });
                }
                outcome.tool_calls.push(ToolCall { tool, mcp });
            }
        }
        Some("result") => {
            *result_seen = true;
            let result = &event["result"];
            outcome.conversation_id = result["conversation_id"].as_str().filter(|id| !id.is_empty()).map(str::to_string).or(outcome.conversation_id.take());
            outcome.text = result["response"].as_str().unwrap_or_default().to_string();
            if let Some(usage) = result.get("usage").filter(|usage| usage.is_object()) {
                outcome.usage = Some(Usage {
                    input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                    total_tokens: usage["total_tokens"].as_u64().unwrap_or(0),
                });
            }
            outcome.denied = result["denied_actions"]
                .as_array()
                .map(|actions| actions.iter().filter_map(|action| action["action"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if result["status"].as_str() != Some("SUCCESS") {
                let message = result["error"].as_str().unwrap_or_default();
                outcome.failure = Some(classify_error(message));
            }
        }
        _ => {}
    }
}

/// 调用一次。提示词走 stdin。出现的任何问题都放进 `Outcome`，不 panic、不吞错。
pub async fn run(opts: &AgyOpts, prompt: &str) -> Outcome {
    let started = Instant::now();
    let mut outcome = Outcome::default();
    let sandbox = match Sandbox::create(opts) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            outcome.failure = Some(Failure::Spawn(format!("无法准备 agy 的临时环境：{}", error.kind())));
            return outcome;
        }
    };
    let mut cmd = command(opts, &sandbox);
    cmd.args(["--print=", "--input-format", "stream-json", "--output-format", "stream-json", "--mode", "plan"]);
    cmd.arg("--settings").arg(sandbox.settings());
    cmd.arg("--log-file").arg(sandbox.log());
    cmd.arg(format!("--print-timeout={}s", opts.total_timeout.as_secs().max(1)));
    if let Some(model) = &opts.model {
        cmd.args(["--model", model]);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            outcome.failure = Some(Failure::Spawn(format!("无法启动 agy 命令行：{}", error.kind())));
            return outcome;
        }
    };
    let pid = child.id();
    let message = json!({"event": "user", "message": {"content": prompt}}).to_string();
    let mut stdin = child.stdin.take().expect("已配置 piped");
    let stdout = child.stdout.take().expect("已配置 piped");
    let mut stderr = child.stderr.take().expect("已配置 piped");

    let work = async {
        // 写完就关闭 stdin：流式输入模式下，它处理完这一轮就退出
        let _ = stdin.write_all(message.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        drop(stdin);
        let mut seen = std::collections::HashSet::new();
        let (mut result_seen, mut total) = (false, 0usize);
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            total += line.len();
            if total > MAX_OUTPUT_BYTES {
                outcome.failure = Some(Failure::Result("agy 的输出超过上限，已中止".into()));
                break;
            }
            if let Ok(event) = serde_json::from_str::<Value>(&line) {
                absorb(&event, opts, &mut outcome, &mut seen, &mut result_seen);
            }
        }
        let mut err = Vec::new();
        let _ = (&mut stderr).take(4096).read_to_end(&mut err).await;
        let status = child.wait().await;
        (result_seen, status, String::from_utf8_lossy(&err).into_owned())
    };
    match tokio::time::timeout(opts.total_timeout, work).await {
        Ok((result_seen, status, err)) => {
            if outcome.failure.is_none() {
                let code = status.ok().and_then(|status| status.code());
                if !result_seen {
                    // 没有 result 事件：stderr 只用来分类，不原样往外传
                    outcome.failure = Some(if code == Some(0) { Failure::NoResult } else if err.is_empty() { Failure::Exit { code } } else { classify_exit(&err, code) });
                } else if code.is_some_and(|code| code != 0) && outcome.text.is_empty() {
                    outcome.failure = Some(Failure::Exit { code });
                }
            }
        }
        Err(_) => {
            kill_group(pid);
            outcome.failure = Some(Failure::Timeout { secs: opts.total_timeout.as_secs() });
        }
    }
    // 不管怎么结束，进程组里不能留下 agy 和它的 MCP 子进程
    kill_group(pid);
    outcome.wall_s = started.elapsed().as_secs_f64();
    outcome
}

/// 没有 result 事件时，用 stderr 判断是登录、额度还是别的问题。
fn classify_exit(stderr: &str, code: Option<i32>) -> Failure {
    match classify_error(stderr) {
        Failure::Result(_) => Failure::Exit { code },
        other => other,
    }
}
