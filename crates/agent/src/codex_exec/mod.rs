//! 经 `codex exec` 调用 GPT（本机 ChatGPT 会员登录）。**只做设备 MCP 工具，不给 shell 和别的任何工具。**
//!
//! 流程：检查会员登录 → 生成锁死的模型目录和命令行 → **运行前审计工具清单** → 带看门狗运行 →
//! 卡住按规则重启 / 续跑 → 收尾清理会话文件。额度用完如实上报，不自动换计费方式。

pub mod audit;
pub mod command;
pub mod events;
pub mod watchdog;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::claude_cli::looks_like_quota;
use command::{Bin, DeviceMcp, Spec};
use events::{handle, Stall, Summary};
use watchdog::{may_resume, Limits, Watch};

/// 续跑时放在任务前面的话：卡住发生在会话早期时，会话里可能还没有这条任务，所以原文一并带上。
pub const RESUME_NOTE: &str = "【会话中断后续跑】刚才与后端的连接卡住了，已重新连接。请先调用一次 observe 看清手机当前页面，然后从你已经完成的进度继续，不要从头重做。原任务如下（如果你已经看过，请忽略重复部分）：\n\n";

#[derive(Debug, Clone)]
pub struct CodexOpts {
    pub bin: Bin,
    /// 模型名来自角色配置，代码里不写死；`None` 用本机目录里的第一个
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub mcp: DeviceMcp,
    /// 允许 Codex 使用的设备工具
    pub allowed: Vec<String>,
    pub limits: Limits,
    /// 整次执行的总时限
    pub timeout: Duration,
    /// 运行前审计工具清单。**除测试外必须开着。**
    pub audit: bool,
    /// 传给 codex 的环境变量（已按白名单过滤）
    pub env: Vec<(String, String)>,
}

impl CodexOpts {
    pub fn new(mcp: DeviceMcp, allowed: Vec<String>) -> Self {
        Self {
            bin: Bin::default(),
            model: None,
            reasoning_effort: None,
            mcp,
            allowed,
            limits: Limits::default(),
            timeout: Duration::from_secs(900),
            audit: true,
            env: command::filter_env(std::env::vars()),
        }
    }
}

/// 一次进程运行的结局。
enum PassEnd {
    Done,
    Stall(Stall),
    /// 出现白名单之外的调用，已停止，不再续跑
    Violation,
    Timeout,
}

/// `agent_log.jsonl` 写入器：每行一个 `{t, kind, ...}`。
struct Log {
    file: std::fs::File,
    start: Instant,
}

impl Log {
    fn write(&mut self, kind: &str, data: Value) {
        let mut obj = json!({ "t": (self.start.elapsed().as_secs_f64() * 10.0).round() / 10.0, "kind": kind });
        if let (Some(o), Some(d)) = (obj.as_object_mut(), data.as_object()) {
            o.extend(d.clone());
        }
        let _ = writeln!(self.file, "{obj}");
        let _ = self.file.flush();
    }
}

/// 杀掉整个进程组：先 SIGTERM，3 秒内没退再 SIGKILL。
async fn terminate(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else { return };
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        // SAFETY: 只发信号；pid 是自己启动、自己进程组的组长
        unsafe { libc::kill(-(pid as i32), sig) };
        if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_ok() {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn one_pass(o: &CodexOpts, args: &[String], work: &Path, prompt: &str, s: &mut Summary, log: &mut Log, deadline: Instant) -> PassEnd {
    let mut cmd = o.bin.command();
    cmd.args(args)
        .arg("-")
        .env_clear()
        .envs(o.env.iter().cloned())
        .current_dir(work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            s.error = Some(format!("无法启动 codex：{e}"));
            return PassEnd::Done;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes()).await;
    }
    // stderr 只留最后 4000 字节，卡住时留现场
    let err_tail: Arc<Mutex<Vec<u8>>> = Arc::default();
    let tail = err_tail.clone();
    let mut stderr = child.stderr.take().expect("已配置 piped");
    let drain = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let mut t = tail.lock().unwrap();
            t.extend_from_slice(&buf[..n]);
            let over = t.len().saturating_sub(4000);
            t.drain(..over);
        }
    });

    let mut lines = BufReader::new(child.stdout.take().expect("已配置 piped")).lines();
    let base = s.tokens.clone().unwrap_or_default();
    let mut watch = Watch::new(Instant::now());
    let mut last = ("进程启动".to_string(), Instant::now());

    let end = loop {
        let now = Instant::now();
        if now >= deadline {
            break PassEnd::Timeout;
        }
        let limit = watch.wait_limit(now, deadline, &o.limits, s.tool_calls == 0);
        let line = match tokio::time::timeout(limit, lines.next_line()).await {
            Err(_) => {
                if Instant::now() >= deadline {
                    break PassEnd::Timeout;
                }
                let kind = watch.stall_kind(&o.limits);
                let tail = String::from_utf8_lossy(&err_tail.lock().unwrap()).to_string();
                log.write(
                    "stall",
                    json!({ "outcome": kind.name(), "silent_s": last.1.elapsed().as_secs_f64().round(), "last_event": last.0,
                            "inflight": watch.inflight.as_ref().map(|(t, _)| t), "stderr_tail": tail.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect::<String>() }),
                );
                break PassEnd::Stall(kind);
            }
            Ok(Ok(None)) => break PassEnd::Done,
            Ok(Err(e)) => {
                s.error = Some(format!("读取 codex 输出失败：{e}"));
                break PassEnd::Done;
            }
            Ok(Ok(Some(l))) => l,
        };
        // 不是 JSON 的行（警告、提示）忽略，但已经算作「还活着」
        let Ok(ev) = serde_json::from_str::<Value>(&line) else { continue };
        let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let item_type = ev.pointer("/item/type").and_then(Value::as_str).unwrap_or("");
        let tool = ev.pointer("/item/tool").and_then(Value::as_str).unwrap_or("");
        let message = ev.get("message").and_then(Value::as_str).unwrap_or("");
        watch.on_event(Instant::now(), kind, item_type, tool, message);
        last = (format!("{kind}/{item_type}/{tool}"), Instant::now());
        let mut pending: Vec<(String, Value)> = vec![];
        let r = handle(&ev, s, &o.allowed, &base, &mut |k, v| pending.push((k.to_string(), v)));
        for (k, v) in pending {
            log.write(&k, v);
        }
        if let Err(msg) = r {
            log.write("violation", json!({ "text": msg }));
            s.error = Some(msg);
            break PassEnd::Violation;
        }
    };

    let mut code = None;
    if matches!(end, PassEnd::Done) {
        code = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(Ok(st)) => st.code(),
            _ => None,
        };
    }
    terminate(&mut child).await; // 无论如何清掉残留的子孙进程
    let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
    if matches!(end, PassEnd::Done) {
        s.exit_code = code;
        if (code != Some(0) || s.num_turns == 0) && s.error.is_none() {
            let tail = String::from_utf8_lossy(&err_tail.lock().unwrap()).trim().to_string();
            s.error = Some(if tail.is_empty() { format!("CLI 未正常完成（退出码 {code:?}）") } else { tail.chars().rev().take(700).collect::<Vec<_>>().into_iter().rev().collect() });
        }
    }
    end
}

/// 带重启 / 续跑的运行循环。
#[allow(clippy::too_many_arguments)]
async fn invoke(o: &CodexOpts, spec: &Spec<'_>, work: &Path, prompt: &str, s: &mut Summary, log: &mut Log) {
    let started = Instant::now();
    let deadline = started + o.timeout;
    let first_args = command::build_args(spec, None);
    let mut args = first_args.clone();
    let mut cur_prompt = prompt.to_string();
    loop {
        match one_pass(o, &args, work, &cur_prompt, s, log, deadline).await {
            PassEnd::Done | PassEnd::Violation => break,
            PassEnd::Timeout => {
                s.error = Some(format!("整体超时（{}s）", o.timeout.as_secs()));
                s.timed_out = true;
                break;
            }
            PassEnd::Stall(kind) => {
                s.stalls.push(kind);
                if !may_resume(s.resumes, s.tool_calls, s.thread_id.is_some()) {
                    s.error = Some(if s.thread_id.is_some() {
                        format!("Codex 卡住（{}），已续跑 {} 次仍未恢复", kind.name(), s.resumes)
                    } else {
                        format!("Codex 卡住（{}），没有拿到会话 id，无法续跑", kind.name())
                    });
                    s.stalled = true;
                    break;
                }
                s.resumes += 1;
                s.error = None; // 上一轮留下的临时错误（如重连失败）不算数，看这一轮的结果
                tokio::time::sleep(o.limits.backoff(s.resumes)).await;
                if s.tool_calls == 0 {
                    // 还没动过手机：干净重来
                    log.write("restart", json!({ "n": s.resumes, "after": kind.name() }));
                    args = first_args.clone();
                    cur_prompt = prompt.to_string();
                } else {
                    let id = s.thread_id.clone().expect("may_resume 已保证有会话 id");
                    log.write("resume", json!({ "n": s.resumes, "after": kind.name(), "thread_id": id }));
                    args = command::build_args(spec, Some(&id));
                    cur_prompt = format!("{RESUME_NOTE}{prompt}");
                }
            }
        }
    }
    s.wall_s = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
}

async fn prepare_and_run(o: &CodexOpts, instructions: &str, prompt: &str, run_dir: &Path, work: &Path, s: &mut Summary) -> Result<(), String> {
    command::check_login(&o.bin, &o.env).await?;
    let (catalog, model) = command::catalog(&o.bin, work, o.model.as_deref(), &o.env).await?;
    s.model = Some(model.clone());
    s.effort = o.reasoning_effort.clone();
    let instr = work.join("instructions.txt");
    std::fs::write(&instr, instructions).map_err(|e| format!("写指令文件失败：{e}"))?;
    let path_env = o.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str()).unwrap_or("/usr/bin:/bin");
    let spec = Spec {
        model: &model,
        work,
        catalog: &catalog,
        instructions: &instr,
        effort: o.reasoning_effort.as_deref(),
        mcp: &o.mcp,
        allowed: &o.allowed,
        path_env,
    };
    if o.audit {
        audit::audit_tools(&o.bin, &command::build_args(&spec, None), &o.allowed, work, path_env).await?;
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(run_dir.join("agent_log.jsonl"))
        .map_err(|e| format!("创建 agent_log.jsonl 失败（同一个目录不能重复执行）：{e}"))?;
    let mut log = Log { file, start: Instant::now() };
    log.write("model", json!({ "model": model, "effort": o.reasoning_effort }));
    if o.reasoning_effort.is_none() {
        // 不传推理强度时 Codex 会落到模型目录里的默认值（通常是 low），而且我们为了锁死工具忽略了用户自己的 Codex 配置，
        // 所以这会让执行员比用户平时用的弱很多。不阻止运行，但必须明说。
        log.write("warning", json!({ "text": "没有指定推理强度，Codex 将使用模型目录里的默认值（通常是 low）；用户自己的 Codex 配置因锁死工具而被忽略" }));
        tracing::warn!("codex 执行员没有指定推理强度，将使用模型目录默认值（通常是 low）");
    }
    invoke(o, &spec, work, prompt, s, &mut log).await;
    Ok(())
}

/// 执行一次。所有问题都放进返回的 `Summary.error`，不 panic；额度用完置 `quota_exhausted`，调用方据此暂停。
pub async fn run_executor(o: &CodexOpts, instructions: &str, prompt: &str, run_dir: &Path) -> Summary {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let work: PathBuf = std::env::temp_dir().join(format!("hafa-gpt-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
    let mut s = Summary::default();
    let r = match std::fs::create_dir_all(&work) {
        Ok(()) => prepare_and_run(o, instructions, prompt, run_dir, &work, &mut s).await,
        Err(e) => Err(format!("创建工作目录失败：{e}")),
    };
    if let Err(e) = r {
        s.error = Some(e);
    }
    command::cleanup_sessions(&command::codex_home_of(&o.env), &s.thread_ids);
    let _ = std::fs::remove_dir_all(&work);
    if s.error.as_deref().is_some_and(looks_like_quota) {
        s.quota_exhausted = true;
    }
    s
}
