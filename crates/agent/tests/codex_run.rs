//! `codex exec` 运行循环的进程级测试：用假的 codex 脚本（由 /bin/sh 读取）模拟各种卡住和失败。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent::codex_exec::command::{Bin, DeviceMcp};
use agent::codex_exec::events::{Stall, Summary};
use agent::codex_exec::watchdog::Limits;
use agent::codex_exec::{run_executor, CodexOpts, RESUME_NOTE};

fn tmp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!("codex_run_{tag}_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ---------------------------------------------------------------- 假脚本片段

const HEAD: &str = r#"
case "$1" in
  login) echo "Logged in using ChatGPT"; exit 0;;
  debug) echo '{"models":[]}'; exit 0;;
esac
# 运行前审计那次调用单独记标记，不计入「真实运行」次数
case "$*" in
  *hafa_audit*) : > "$0.audit_seen";;
  *)
    echo "$@" >> "$0.args"
    cat >> "$0.stdin"; echo "-----" >> "$0.stdin"
    n=$(cat "$0.count" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$0.count";;
esac
"#;
const THREAD: &str = r#"echo '{"type":"thread.started","thread_id":"aaaabbbb"}'"#;
const TURN: &str = r#"echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}'"#;

fn tool(name: &str) -> String {
    format!(
        r#"echo '{{"type":"item.started","item":{{"type":"mcp_tool_call","server":"device","tool":"{name}"}}}}'
echo '{{"type":"item.completed","item":{{"type":"mcp_tool_call","server":"device","tool":"{name}","status":"completed"}}}}'"#
    )
}

fn tool_start(name: &str) -> String {
    format!(r#"echo '{{"type":"item.started","item":{{"type":"mcp_tool_call","server":"device","tool":"{name}"}}}}'"#)
}

struct Fx {
    opts: CodexOpts,
    script: PathBuf,
    run_dir: PathBuf,
    home: PathBuf,
}

/// 小时限：卡住 / 工具挂住 0.8 秒，重连 3 条判坏，退避 10ms。
fn small() -> Limits {
    // 阈值不能太紧：续跑要重新起进程、跑模型目录和审计，整个工作区并行测试时曾因 800ms 偶发多判一次卡住
    Limits { stall: Duration::from_millis(2500), start_stall: Duration::from_millis(2500), tool_hang: Duration::from_millis(2500), reconnect_limit: 3, backoff_unit: Duration::from_millis(10) }
}

fn fx(body: &str) -> Fx {
    let dir = tmp("s");
    let script = dir.join("codex.sh");
    std::fs::write(&script, format!("{HEAD}\n{body}\n")).unwrap();
    let home = tmp("home");
    std::fs::write(home.join("models_cache.json"), r#"{"models":[{"slug":"test-model"}]}"#).unwrap();
    let mcp = DeviceMcp { command: "/bin/mcp-device".into(), env: vec![("HAFA_SERIAL".into(), "S1".into())] };
    let mut opts = CodexOpts::new(mcp, ["observe", "tap", "finish", "need_human"].map(String::from).into());
    opts.bin = Bin { program: "/bin/sh".into(), pre: vec![script.to_string_lossy().into()] };
    opts.limits = small();
    opts.audit = false;
    opts.timeout = Duration::from_secs(30);
    opts.env = vec![("HOME".into(), home.to_string_lossy().into()), ("PATH".into(), "/usr/bin:/bin".into()), ("CODEX_HOME".into(), home.to_string_lossy().into())];
    Fx { opts, script, run_dir: tmp("run"), home }
}

impl Fx {
    async fn run(&self) -> Summary {
        run_executor(&self.opts, "指令", "任务正文", &self.run_dir).await
    }
    fn read(&self, suffix: &str) -> String {
        std::fs::read_to_string(format!("{}{}", self.script.display(), suffix)).unwrap_or_default()
    }
    fn audited(&self) -> bool {
        std::path::Path::new(&format!("{}.audit_seen", self.script.display())).exists()
    }
    fn calls(&self) -> usize {
        self.read(".count").trim().parse().unwrap_or(0)
    }
}

// ---------------------------------------------------------------- 测试

#[tokio::test]
async fn 正常完成() {
    let f = fx(&format!("{THREAD}\n{}\n{}\n{TURN}", tool("observe"), tool("finish")));
    let s = f.run().await;
    assert_eq!(s.error, None);
    assert_eq!((s.tool_calls, s.num_turns, s.exit_code, s.resumes), (2, 1, Some(0), 0));
    assert_eq!(s.thread_id.as_deref(), Some("aaaabbbb"));
    assert_eq!(s.tokens.as_ref().map(|t| (t.input, t.output)), Some((100, 10)));
    let log = std::fs::read_to_string(f.run_dir.join("agent_log.jsonl")).unwrap();
    assert!(log.contains("\"kind\":\"tool_use\""), "{log}");
    assert!(f.read(".stdin").contains("任务正文"), "任务正文要走 stdin");
}

#[tokio::test]
async fn 只传白名单环境变量() {
    let f = fx(&format!("env > \"$0.env\"\n{THREAD}\n{TURN}"));
    let s = f.run().await;
    assert_eq!(s.error, None);
    let env = f.read(".env");
    assert!(env.contains("CODEX_HOME=") && env.contains("HOME="));
    // 测试进程自己一定有 USER / SHELL；子进程里不应该有，说明环境被清空后只放行了白名单
    assert!(!env.contains("\nUSER=") && !env.starts_with("USER=") && !env.contains("\nSHELL="), "非白名单变量泄露给了 codex：{env}");
}

#[tokio::test]
async fn 动过手机后卡住_用会话id续跑() {
    let f = fx(&format!(
        r#"if [ "$2" = "resume" ]; then
  {}
  {TURN}
  exit 0
fi
{THREAD}
{}
sleep 30"#,
        tool("finish"),
        tool("observe")
    ));
    let s = f.run().await;
    assert_eq!(s.error, None, "续跑成功后临时错误不算数");
    assert_eq!((s.resumes, s.stalls.clone()), (1, vec![Stall::Stalled]));
    let args = f.read(".args");
    let second = args.lines().nth(1).expect("应有第二次调用");
    assert!(second.contains("resume") && second.contains("aaaabbbb"), "第二次必须是 resume + 会话 id：{second}");
    assert!(f.read(".stdin").contains(RESUME_NOTE.trim_end()), "续跑要带上「会话中断后续跑」说明");
    assert!(f.read(".stdin").matches("任务正文").count() >= 2, "续跑时任务原文要一并带上");
    let log = std::fs::read_to_string(f.run_dir.join("agent_log.jsonl")).unwrap();
    assert!(log.contains("\"kind\":\"resume\"") && log.contains("\"kind\":\"stall\""), "{log}");
}

#[tokio::test]
async fn 还没动手机就卡住_干净重启而不是续跑() {
    let f = fx(&format!(
        r#"if [ "$n" = "1" ]; then {THREAD}; sleep 30; fi
{}
{TURN}"#,
        tool("observe")
    ));
    let s = f.run().await;
    assert_eq!(s.error, None);
    assert_eq!((s.resumes, s.stalls.clone()), (1, vec![Stall::Stalled]));
    let args = f.read(".args");
    assert!(!args.lines().nth(1).unwrap().contains("resume"), "还没动过手机：从头重来，不是 resume");
    let log = std::fs::read_to_string(f.run_dir.join("agent_log.jsonl")).unwrap();
    assert!(log.contains("\"kind\":\"restart\""), "{log}");
}

#[tokio::test]
async fn 动过手机后一直卡住_续跑两次后放弃() {
    let f = fx(&format!("{THREAD}\n{}\nsleep 30", tool("observe")));
    let s = f.run().await;
    assert!(s.stalled && s.error.as_deref().unwrap().contains("已续跑 2 次仍未恢复"), "{:?}", s.error);
    assert_eq!(s.stalls.len(), 3, "第一次 + 两次续跑");
    assert_eq!(f.calls(), 3);
}

#[tokio::test]
async fn 没动手机一直卡住_多给一次重试() {
    let f = fx(&format!("{THREAD}\nsleep 30"));
    let s = f.run().await;
    assert!(s.stalled && s.error.as_deref().unwrap().contains("已续跑 3 次仍未恢复"), "{:?}", s.error);
    assert_eq!(f.calls(), 4, "还没动手机：初次 + 3 次重试");
}

#[tokio::test]
async fn 没拿到会话id无法续跑() {
    let f = fx("sleep 30");
    let s = f.run().await;
    assert!(s.stalled && s.error.as_deref().unwrap().contains("没有拿到会话 id，无法续跑"), "{:?}", s.error);
    assert_eq!(f.calls(), 1, "无法续跑就不要再启动");
}

#[tokio::test]
async fn 工具调用挂住_判tool_hang再续跑() {
    let f = fx(&format!(
        r#"if [ "$2" = "resume" ]; then {}; {TURN}; exit 0; fi
{THREAD}
{}
sleep 30"#,
        tool("finish"),
        tool_start("tap")
    ));
    let s = f.run().await;
    assert_eq!(s.error, None);
    assert_eq!(s.stalls, vec![Stall::ToolHang]);
}

#[tokio::test]
async fn need_human期间不算卡住() {
    // 工具挂住时限 0.8 秒，need_human 等了 1.6 秒，仍不能判卡住
    let f = fx(&format!(
        r#"{THREAD}
{}
sleep 1.6
echo '{{"type":"item.completed","item":{{"type":"mcp_tool_call","server":"device","tool":"need_human","status":"completed"}}}}'
{TURN}"#,
        tool_start("need_human")
    ));
    let s = f.run().await;
    assert_eq!(s.error, None);
    assert!(s.stalls.is_empty() && s.resumes == 0, "等人工被误判成卡住：{:?}", s.stalls);
}

#[tokio::test]
async fn 连续重连提示_立刻判连接已坏而不是干等() {
    let mut f = fx(&format!(
        r#"if [ "$n" = "1" ]; then
  {THREAD}
  for i in 1 2 3; do echo '{{"type":"error","message":"Reconnecting... '$i'/5"}}'; done
  sleep 30
fi
{}
{TURN}"#,
        tool("observe")
    ));
    f.opts.limits.stall = Duration::from_secs(20); // 如果干等 stall 就会超过 20 秒
    f.opts.limits.start_stall = Duration::from_secs(20);
    let t = Instant::now();
    let s = f.run().await;
    assert_eq!(s.stalls, vec![Stall::ReconnectLoop]);
    assert!(t.elapsed() < Duration::from_secs(10), "连接已坏应立即判定，实际 {:?}", t.elapsed());
}

#[tokio::test]
async fn 白名单之外的调用立即停止且不再续跑() {
    let f = fx(&format!(
        r#"{THREAD}
echo '{{"type":"item.started","item":{{"type":"command_execution","command":"ls"}}}}'
sleep 30"#
    ));
    let t = Instant::now();
    let s = f.run().await;
    assert!(s.error.as_deref().unwrap().contains("非设备工具事件"), "{:?}", s.error);
    assert_eq!((s.resumes, f.calls()), (0, 1), "越界不能当作卡住去续跑");
    assert!(t.elapsed() < Duration::from_secs(10), "越界后要立刻杀进程，实际 {:?}", t.elapsed());
    let log = std::fs::read_to_string(f.run_dir.join("agent_log.jsonl")).unwrap();
    assert!(log.contains("\"kind\":\"violation\""));
}

#[tokio::test]
async fn 不在白名单里的设备工具也停止() {
    let f = fx(&format!("{THREAD}\n{}\nsleep 30", tool_start("input_text")));
    let s = f.run().await;
    assert!(s.error.as_deref().unwrap().contains("device.input_text"), "{:?}", s.error);
}

#[tokio::test]
async fn 额度用完如实上报() {
    let f = fx(&format!(
        r#"{THREAD}
echo '{{"type":"error","message":"You have hit your usage limit"}}'
echo '{{"type":"turn.failed","error":{{"message":"You have hit your usage limit"}}}}'
exit 1"#
    ));
    let s = f.run().await;
    assert!(s.quota_exhausted, "{s:?}");
    assert!(s.error.as_deref().unwrap().contains("usage limit"));
    assert_eq!(f.calls(), 1, "额度用完不重试");
}

#[tokio::test]
async fn 整体超时() {
    let mut f = fx(r#"while true; do echo '{"type":"item.completed","item":{"type":"agent_message"}}'; sleep 0.1; done"#);
    f.opts.timeout = Duration::from_millis(1500);
    f.opts.limits.stall = Duration::from_secs(10);
    f.opts.limits.start_stall = Duration::from_secs(10);
    let t = Instant::now();
    let s = f.run().await;
    assert!(s.timed_out && s.error.as_deref().unwrap().contains("整体超时"), "{:?}", s.error);
    assert!(t.elapsed() < Duration::from_secs(10), "超时后进程要被杀掉，实际 {:?}", t.elapsed());
}

#[tokio::test]
async fn 进程异常退出带出原始stderr() {
    let f = fx("echo '这是原始错误' >&2\nexit 3");
    let s = f.run().await;
    assert_eq!(s.exit_code, Some(3));
    assert!(s.error.as_deref().unwrap().contains("这是原始错误"), "{:?}", s.error);
}

#[tokio::test]
async fn 退出码0但一轮都没完成也算失败() {
    let f = fx(THREAD);
    let s = f.run().await;
    assert!(s.error.is_some(), "没有 turn.completed 不能当成功");
}

#[tokio::test]
async fn 没登录会员就不运行() {
    let dir = tmp("nologin");
    let script = dir.join("codex.sh");
    std::fs::write(&script, "case \"$1\" in login) echo 'Not logged in'; exit 1;; esac\necho ran >> \"$0.ran\"\n").unwrap();
    let mut f = fx("");
    f.opts.bin = Bin { program: "/bin/sh".into(), pre: vec![script.to_string_lossy().into()] };
    let s = f.run().await;
    assert!(s.error.as_deref().unwrap().contains("未检测到 ChatGPT 会员登录"), "{:?}", s.error);
    assert!(!dir.join("codex.sh.ran").exists(), "没登录时不能启动执行");
    assert!(!f.run_dir.join("agent_log.jsonl").exists());
}

#[tokio::test]
async fn 配置的模型找不到() {
    let mut f = fx("");
    f.opts.model = Some("不存在的模型".into());
    let s = f.run().await;
    assert!(s.error.as_deref().unwrap().contains("找不到配置的模型"), "{:?}", s.error);
    assert_eq!(f.calls(), 0);
}

#[tokio::test]
async fn 同一个目录不能重复执行() {
    let f = fx(&format!("{THREAD}\n{TURN}"));
    assert_eq!(f.run().await.error, None);
    let s2 = f.run().await;
    assert!(s2.error.as_deref().unwrap().contains("不能重复执行"), "{:?}", s2.error);
}

#[tokio::test]
async fn 收尾清理本次会话文件() {
    let f = fx(&format!("{THREAD}\n{TURN}"));
    let d = f.home.join("sessions/2026/11");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("rollout-aaaabbbb.jsonl"), "x").unwrap();
    std::fs::write(d.join("rollout-ccccdddd.jsonl"), "x").unwrap();
    f.run().await;
    assert!(!d.join("rollout-aaaabbbb.jsonl").exists(), "本次会话文件要清掉，不在用户的 Codex 历史里留垃圾");
    assert!(d.join("rollout-ccccdddd.jsonl").exists(), "别的会话不能动");
}

// ---------------------------------------------------------------- 运行前审计

fn audit_script(tools_json: &str, extra_curl_args: &str) -> String {
    format!(
        r#"case "$*" in *hafa_audit*)
  for a in "$@"; do case "$a" in model_providers.hafa_audit.base_url=*) url=$(echo "${{a#*=}}" | sed 's/"//g');; esac; done
  curl -s -m 5 -X POST {extra_curl_args} -H 'Content-Type: application/json' -d '{tools_json}' "$url/responses" >/dev/null
  exit 0;;
esac
{THREAD}
{TURN}"#
    )
}

const TOOLS_OK: &str = r#"{"tools":[{"type":"namespace","name":"mcp__device","tools":[{"name":"observe"},{"name":"tap"},{"name":"finish"},{"name":"need_human"}]}]}"#;
const TOOLS_WITH_SHELL: &str = r#"{"tools":[{"type":"namespace","name":"mcp__device","tools":[{"name":"observe"},{"name":"tap"},{"name":"finish"},{"name":"need_human"}]},{"name":"shell"}]}"#;

#[tokio::test]
async fn 审计通过才真正运行() {
    let mut f = fx(&audit_script(TOOLS_OK, ""));
    f.opts.audit = true;
    let s = f.run().await;
    assert_eq!(s.error, None, "{:?}", s.error);
    assert!(f.audited(), "必须先审计");
    assert_eq!(f.calls(), 1, "审计通过后只有一次真实运行");
}

#[tokio::test]
async fn 审计发现多了shell就在调用模型前停止() {
    let mut f = fx(&audit_script(TOOLS_WITH_SHELL, ""));
    f.opts.audit = true;
    let s = f.run().await;
    assert!(s.error.as_deref().unwrap().contains("工具清单不符") && s.error.as_deref().unwrap().contains("shell"), "{:?}", s.error);
    assert!(f.audited(), "审计确实跑了");
    assert_eq!(f.calls(), 0, "审计不通过，真实运行一次都不能发生");
    assert!(!f.run_dir.join("agent_log.jsonl").exists());
}

#[tokio::test]
async fn 审计端点拒绝带凭证的请求() {
    let mut f = fx(&audit_script(TOOLS_OK, "-H 'Authorization: Bearer secret'"));
    f.opts.audit = true;
    let s = f.run().await;
    assert!(s.error.as_deref().unwrap().contains("预检未完成"), "带 Authorization 的请求必须被拒绝，不能当成有效清单：{:?}", s.error);
    assert!(f.audited());
    assert_eq!(f.calls(), 0, "没拿到有效清单就不能真实运行");
}

// ---------------------------------------------------------------- 记录实际用的模型和推理强度

#[tokio::test]
async fn 摘要记下实际用的模型和推理强度() {
    let mut f = fx(&format!("{THREAD}\n{TURN}"));
    f.opts.reasoning_effort = Some("max".into());
    let s = f.run().await;
    assert_eq!(s.error, None);
    assert_eq!((s.model.as_deref(), s.effort.as_deref()), (Some("test-model"), Some("max")), "怀疑降智时要能查到当时实际用的模型和强度");
    assert!(f.read(".args").contains("model_reasoning_effort=\"max\""), "指定的推理强度必须真的传给 codex：{}", f.read(".args"));
    let log = std::fs::read_to_string(f.run_dir.join("agent_log.jsonl")).unwrap();
    assert!(log.contains("\"kind\":\"model\"") && log.contains("\"effort\":\"max\""), "{log}");
    assert!(!log.contains("\"kind\":\"warning\""), "指定了强度就不该有警告");
}

#[tokio::test]
async fn 没指定推理强度要明确警告而不是悄悄降到最低() {
    let f = fx(&format!("{THREAD}\n{TURN}")); // 默认 reasoning_effort = None
    let s = f.run().await;
    assert_eq!(s.effort, None);
    assert!(!f.read(".args").contains("model_reasoning_effort"), "没指定就不传（交给 codex 默认），但必须明说");
    let log = std::fs::read_to_string(f.run_dir.join("agent_log.jsonl")).unwrap();
    assert!(log.contains("\"kind\":\"warning\"") && log.contains("通常是 low"), "要警告「没指定强度会落到默认的 low」：{log}");
}
