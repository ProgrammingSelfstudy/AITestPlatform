//! 解析 `codex exec --json` 的事件流，维护一次执行的摘要。行为来自技术验证里已验证过的实现。
//!
//! 安全关键：**只允许设备 MCP 的白名单工具**。出现任何别的工具调用（别的 MCP、shell、读写文件……）
//! 立即返回错误，调用方据此停止进程——不能靠「配置里已经关掉了」就放心。

use serde_json::Value;

use crate::claude_cli::looks_like_quota;

/// 单条错误信息最多保留的字符数。
const MAX_ERR_CHARS: usize = 700;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub reasoning_output: u64,
}

impl Tokens {
    fn add(&self, o: &Tokens) -> Tokens {
        Tokens {
            input: self.input + o.input,
            output: self.output + o.output,
            cache_read: self.cache_read + o.cache_read,
            reasoning_output: self.reasoning_output + o.reasoning_output,
        }
    }
}

/// 卡住的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    /// 模型太久没有任何进展
    Stalled,
    /// 工具调用发出后太久没返回（设备服务挂住）
    ToolHang,
    /// 连续太多条 Reconnecting 提示，连接已经坏了
    ReconnectLoop,
}

impl Stall {
    pub fn name(self) -> &'static str {
        match self {
            Stall::Stalled => "stalled",
            Stall::ToolHang => "tool_hang",
            Stall::ReconnectLoop => "reconnect_loop",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// 最近一次会话 id，卡住后靠它续跑
    pub thread_id: Option<String>,
    /// 重启会产生新会话，收尾时全部清理
    pub thread_ids: Vec<String>,
    pub error: Option<String>,
    /// 实际用的模型（来自本机模型目录）和推理强度，每次执行都要记下来：
    /// 怀疑模型被降级、或者结果比预期差时，才有据可查
    pub model: Option<String>,
    pub effort: Option<String>,
    pub quota_exhausted: bool,
    pub tokens: Option<Tokens>,
    pub num_turns: u32,
    pub tool_calls: u32,
    pub image_results: u32,
    pub exit_code: Option<i32>,
    pub resumes: u32,
    pub stalls: Vec<Stall>,
    pub stalled: bool,
    pub timed_out: bool,
    pub wall_s: f64,
}

fn clip(s: &str) -> String {
    s.chars().take(MAX_ERR_CHARS).collect()
}

/// 处理一条事件。`allowed` 是允许的设备工具名；`base` 是之前会话的用量（续跑后是新会话，要加回来）；
/// `log` 写 `agent_log.jsonl`。返回 `Err` 表示必须立刻停止（白名单之外的调用）。
pub fn handle(ev: &Value, s: &mut Summary, allowed: &[String], base: &Tokens, log: &mut dyn FnMut(&str, Value)) -> Result<(), String> {
    let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
    let item = ev.get("item").cloned().unwrap_or(Value::Null);

    match kind {
        "thread.started" => {
            if let Some(id) = ev.get("thread_id").and_then(Value::as_str) {
                s.thread_id = Some(id.to_string());
                s.thread_ids.push(id.to_string());
            }
            return Ok(());
        }
        "error" | "turn.failed" => {
            let msg = ev
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| ev.get("message").and_then(Value::as_str))
                .unwrap_or("CLI 返回错误");
            let msg = clip(msg);
            log("driver_error", serde_json::json!({ "text": msg }));
            // Reconnecting... 只是临时提示，不算最终错误；turn.failed 一律算
            if kind == "turn.failed" || !msg.starts_with("Reconnecting...") {
                s.error = Some(msg.clone());
            }
            if looks_like_quota(&msg) {
                s.quota_exhausted = true;
            }
            return Ok(());
        }
        "turn.completed" => {
            let u = ev.get("usage").cloned().unwrap_or(Value::Null);
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            let now = Tokens {
                input: n("input_tokens"),
                output: n("output_tokens"),
                cache_read: n("cached_input_tokens"),
                reasoning_output: n("reasoning_output_tokens"),
            };
            s.tokens = Some(now.add(base));
            s.num_turns += 1;
        }
        _ => {}
    }

    if kind != "item.started" && kind != "item.completed" {
        return Ok(());
    }
    let typ = item.get("type").and_then(Value::as_str).unwrap_or("");
    match typ {
        "mcp_tool_call" => {
            let server = item.get("server").and_then(Value::as_str).unwrap_or("");
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("");
            if server != "device" || !allowed.iter().any(|a| a == tool) {
                return Err(format!("设备白名单之外的调用，停止：{server}.{tool}"));
            }
            if kind == "item.started" {
                s.tool_calls += 1;
                log("tool_use", serde_json::json!({ "tool": tool }));
            } else {
                let content = item.pointer("/result/content").and_then(Value::as_array).cloned().unwrap_or_default();
                let images = content.iter().filter(|p| p.get("type").and_then(Value::as_str) == Some("image")).count();
                s.image_results += images as u32;
                let mut detail = item.pointer("/error/message").and_then(Value::as_str).map(str::to_string);
                if item.get("status").and_then(Value::as_str) == Some("failed") && detail.is_none() {
                    let texts: Vec<&str> = content.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect();
                    detail = Some(texts.join(" "));
                }
                let detail: Option<String> = detail.filter(|d| !d.is_empty()).map(|d| d.chars().take(500).collect());
                log(
                    "tool_result",
                    serde_json::json!({ "tool": tool, "images": images, "status": item.get("status"), "error": detail }),
                );
            }
        }
        "error" => {
            // Codex 的警告类提示（降级到 HTTPS、模型元数据缺失等），不是工具调用，只记录不中止
            let m = item.get("message").and_then(Value::as_str).unwrap_or("");
            log("driver_warning", serde_json::json!({ "text": m.chars().take(300).collect::<String>() }));
            if looks_like_quota(m) {
                s.quota_exhausted = true;
            }
        }
        "agent_message" | "reasoning" => {}
        other => return Err(format!("非设备工具事件 {other}，已停止。")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn allowed() -> Vec<String> {
        ["observe", "tap", "finish"].map(String::from).into()
    }

    /// 喂一串事件，返回 (摘要, 日志条数, 第一个错误)
    fn feed(events: &[Value]) -> (Summary, Vec<String>, Option<String>) {
        let mut s = Summary::default();
        let mut logs = vec![];
        for e in events {
            if let Err(m) = handle(e, &mut s, &allowed(), &Tokens::default(), &mut |k, _| logs.push(k.to_string())) {
                return (s, logs, Some(m));
            }
        }
        (s, logs, None)
    }

    fn call(kind: &str, server: &str, tool: &str) -> Value {
        json!({"type": kind, "item": {"type": "mcp_tool_call", "server": server, "tool": tool}})
    }

    #[test]
    fn 记录会话id() {
        let (s, _, e) = feed(&[json!({"type":"thread.started","thread_id":"abc-123"})]);
        assert!(e.is_none());
        assert_eq!((s.thread_id.as_deref(), s.thread_ids.len()), (Some("abc-123"), 1));
    }

    #[test]
    fn 重启产生新会话时全部记下以便清理() {
        let (s, _, _) = feed(&[json!({"type":"thread.started","thread_id":"a1"}), json!({"type":"thread.started","thread_id":"b2"})]);
        assert_eq!((s.thread_id.as_deref(), s.thread_ids.as_slice()), (Some("b2"), &["a1".to_string(), "b2".to_string()][..]));
    }

    #[test]
    fn 设备白名单内的调用计数() {
        let (s, logs, e) = feed(&[call("item.started", "device", "observe"), call("item.completed", "device", "observe")]);
        assert!(e.is_none());
        assert_eq!(s.tool_calls, 1, "只在 started 时计一次");
        assert_eq!(logs, ["tool_use", "tool_result"]);
    }

    #[test]
    fn 白名单之外的设备工具立即停止() {
        let (_, _, e) = feed(&[call("item.started", "device", "input_text")]);
        assert!(e.unwrap().contains("设备白名单之外的调用"), "没在 allowed 里的工具不能放行");
    }

    #[test]
    fn 别的mcp服务立即停止() {
        let (_, _, e) = feed(&[call("item.started", "filesystem", "observe")]);
        assert!(e.unwrap().contains("filesystem.observe"), "工具名对得上但服务不是 device，也必须停");
    }

    #[test]
    fn shell等非设备事件立即停止() {
        for typ in ["command_execution", "file_change", "web_search", "unknown_new_thing"] {
            let (_, _, e) = feed(&[json!({"type":"item.started","item":{"type":typ}})]);
            assert!(e.unwrap().contains("非设备工具事件"), "{typ} 必须停止");
        }
    }

    #[test]
    fn 模型文字和推理不算违规() {
        let (_, _, e) = feed(&[json!({"type":"item.completed","item":{"type":"agent_message","text":"好"}}), json!({"type":"item.completed","item":{"type":"reasoning"}})]);
        assert!(e.is_none());
    }

    #[test]
    fn 警告类error项只记录不中止() {
        let (s, logs, e) = feed(&[json!({"type":"item.completed","item":{"type":"error","message":"模型元数据缺失，已降级"}})]);
        assert!(e.is_none() && s.error.is_none());
        assert_eq!(logs, ["driver_warning"]);
    }

    #[test]
    fn 重连提示不算最终错误但turn_failed算() {
        let (s, _, _) = feed(&[json!({"type":"error","message":"Reconnecting... 2/5"})]);
        assert!(s.error.is_none(), "重连提示是临时的");
        let (s, _, _) = feed(&[json!({"type":"turn.failed","error":{"message":"Reconnecting... 5/5"}})]);
        assert!(s.error.is_some(), "turn.failed 一律是最终错误");
        let (s, _, _) = feed(&[json!({"type":"error","message":"连接被重置"})]);
        assert_eq!(s.error.as_deref(), Some("连接被重置"));
    }

    #[test]
    fn 额度错误被识别() {
        let (s, _, _) = feed(&[json!({"type":"error","message":"You've hit your usage limit"})]);
        assert!(s.quota_exhausted);
        let (s, _, _) = feed(&[json!({"type":"item.completed","item":{"type":"error","message":"rate limit exceeded"}})]);
        assert!(s.quota_exhausted, "警告项里的额度提示也要识别");
    }

    #[test]
    fn 错误信息按字符截断而不是字节() {
        let long = "错".repeat(1000);
        let (s, _, _) = feed(&[json!({"type":"error","message":long})]);
        assert_eq!(s.error.unwrap().chars().count(), 700, "按字符截断，不能把中文截在字节中间");
    }

    #[test]
    fn 用量累加续跑前的会话() {
        let mut s = Summary::default();
        let base = Tokens { input: 100, output: 10, cache_read: 50, reasoning_output: 5 };
        handle(&json!({"type":"turn.completed","usage":{"input_tokens":30,"output_tokens":3,"cached_input_tokens":20,"reasoning_output_tokens":2}}), &mut s, &allowed(), &base, &mut |_, _| {}).unwrap();
        assert_eq!(s.tokens, Some(Tokens { input: 130, output: 13, cache_read: 70, reasoning_output: 7 }));
        assert_eq!(s.num_turns, 1);
    }

    #[test]
    fn 图片结果和失败详情() {
        let ev = json!({"type":"item.completed","item":{"type":"mcp_tool_call","server":"device","tool":"observe","status":"failed",
            "result":{"content":[{"type":"text","text":"设备命令失败：device offline"},{"type":"image"}]}}});
        let mut s = Summary::default();
        let mut got = None;
        handle(&ev, &mut s, &allowed(), &Tokens::default(), &mut |k, v| if k == "tool_result" { got = Some(v) }).unwrap();
        assert_eq!(s.image_results, 1);
        let v = got.unwrap();
        assert_eq!(v["images"], 1);
        assert!(v["error"].as_str().unwrap().contains("device offline"), "失败时要把工具返回的文字当作详情：{v}");
    }

    #[test]
    fn 无关事件被忽略() {
        let (s, logs, e) = feed(&[json!({"type":"turn.started"}), json!({"type":"item.started","item":{"type":"reasoning"}}), json!({})]);
        assert!(e.is_none() && logs.is_empty() && s.tool_calls == 0);
    }
}
