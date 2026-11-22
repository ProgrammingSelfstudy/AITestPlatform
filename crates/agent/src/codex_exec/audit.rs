//! 运行前的工具清单审计：用和真实运行相同的命令行、模型目录和 MCP 参数启动 codex，
//! 但把模型端点指到本机回环地址，读出它**实际声明**的工具清单，必须恰好等于设备工具白名单。
//! 不传用户凭证、用例或图片；本地端点收到任何带 `Authorization` 的请求都拒绝。
//!
//! 为什么要审计：配置写得再严，Codex 版本升级后默认行为可能变（比如新增内置工具）。
//! 这一步在调用真实模型之前就能发现，而不是等 GPT 已经拿到 shell 才知道。

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::command::Bin;

/// 资源协议自带的三个工具，不算越界。
const RESOURCE_PROTOCOL: [&str; 3] = ["list_mcp_resource_templates", "list_mcp_resources", "read_mcp_resource"];

/// 把请求里的 `tools` 展开成名字（命名空间展开成 `命名空间.名字`）。
pub fn tool_names(tools: &[Value]) -> Vec<String> {
    let mut out = vec![];
    for t in tools {
        if t.get("type").and_then(Value::as_str) == Some("namespace") {
            let ns = t.get("name").and_then(Value::as_str).unwrap_or("");
            let inner = t.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
            out.extend(tool_names(&inner).into_iter().map(|n| format!("{ns}.{n}")));
        } else {
            let n = t.get("name").and_then(Value::as_str).or_else(|| t.get("type").and_then(Value::as_str)).unwrap_or("?");
            out.push(n.to_string());
        }
    }
    out
}

/// 实际声明的工具清单是否恰好等于白名单（`mcp__device__<工具名>`）。
pub fn check_names(names: &[String], allowed: &[String]) -> Result<(), String> {
    let expected: BTreeSet<String> = allowed.iter().map(|a| format!("mcp__device__{a}")).collect();
    let got: BTreeSet<String> = names
        .iter()
        .map(|n| n.replace("mcp__device.", "mcp__device__"))
        .filter(|n| !RESOURCE_PROTOCOL.contains(&n.as_str()))
        .collect();
    if got != expected {
        let mut sorted = names.to_vec();
        sorted.sort();
        return Err(format!("Codex 工具清单不符，已在调用模型前停止。实际：{sorted:?}；要求：{expected:?}"));
    }
    Ok(())
}

/// 读一个 HTTP 请求，返回 (路径, 是否带 Authorization, 请求体)。只为这个审计场景够用，不是通用 HTTP 解析。
async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<(String, bool, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();
    let has_auth = head.lines().any(|l| l.to_lowercase().starts_with("authorization:"));
    let len: usize = head.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse().ok()))?;
    if len == 0 || len > 4_000_000 {
        return None;
    }
    while buf.len() < head_end + len {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some((path, has_auth, buf[head_end..head_end + len].to_vec()))
}

/// 审计。`args` 是真实运行用的参数（不含末尾的 `-`）；返回实际声明的工具名。
pub async fn audit_tools(bin: &Bin, args: &[String], allowed: &[String], work: &Path, path_env: &str) -> Result<Vec<String>, String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| format!("审计端点启动失败：{e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let seen: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
    let seen2 = seen.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let reply: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            match read_request(&mut sock).await {
                // 只接受不带凭证、发给 /v1/responses 的请求；其它一概 403
                Some((path, false, body)) if path == "/v1/responses" => {
                    if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                        let tools = v.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
                        *seen2.lock().unwrap() = Some(tool_names(&tools));
                    }
                    let _ = sock.write_all(reply).await;
                }
                _ => {
                    let _ = sock.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                }
            }
        }
    });

    let home = work.join("audit_home");
    std::fs::create_dir_all(&home).map_err(|e| format!("创建审计目录失败：{e}"))?;
    let h = home.to_string_lossy().into_owned();
    let mut cmd = bin.command();
    cmd.args(args)
        .args(["-c", "model_provider=\"hafa_audit\"", "-c", "model_providers.hafa_audit.name=\"本地工具清单\""])
        .args(["-c", &format!("model_providers.hafa_audit.base_url=\"http://127.0.0.1:{port}/v1\"")])
        .args(["-c", "model_providers.hafa_audit.wire_api=\"responses\"", "-c", "model_providers.hafa_audit.stream_max_retries=0"])
        .arg("工具清单检查")
        .env_clear()
        .env("PATH", path_env)
        .env("HOME", &h)
        .env("CODEX_HOME", &h)
        .env("NO_PROXY", "*")
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("审计进程启动失败：{e}"))?;

    // 等到收到清单，或者进程自己退出，最多 40 秒
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while tokio::time::Instant::now() < deadline {
        if seen.lock().unwrap().is_some() || matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if let Some(pid) = child.id() {
        // SAFETY: 只发信号；pid 是刚启动、自己进程组的组长
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    let out = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await;
    server.abort();

    let names = seen.lock().unwrap().clone();
    let Some(names) = names else {
        let tail = out.ok().and_then(Result::ok).map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))).unwrap_or_default();
        let tail: String = tail.chars().rev().take(1200).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(format!("本地工具清单预检未完成：{tail}"));
    };
    check_names(&names, allowed)?;
    let mut sorted = names;
    sorted.sort();
    Ok(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn allowed() -> Vec<String> {
        ["observe", "tap"].map(String::from).into()
    }

    #[test]
    fn 命名空间展开() {
        let tools = [json!({"type":"namespace","name":"mcp__device","tools":[{"name":"observe"},{"name":"tap"}]}), json!({"name":"shell"})];
        assert_eq!(tool_names(&tools), ["mcp__device.observe", "mcp__device.tap", "shell"]);
    }

    #[test]
    fn 没有名字就用类型() {
        assert_eq!(tool_names(&[json!({"type":"web_search"})]), ["web_search"]);
    }

    #[test]
    fn 清单恰好等于白名单才通过() {
        let ok = ["mcp__device__observe".to_string(), "mcp__device__tap".to_string()];
        assert!(check_names(&ok, &allowed()).is_ok());
        let dotted = ["mcp__device.observe".to_string(), "mcp__device.tap".to_string()];
        assert!(check_names(&dotted, &allowed()).is_ok(), "命名空间写法 mcp__device.xxx 也要认");
    }

    #[test]
    fn 多出shell必须拒绝() {
        let n = ["mcp__device__observe".to_string(), "mcp__device__tap".to_string(), "shell".to_string()];
        let e = check_names(&n, &allowed()).unwrap_err();
        assert!(e.contains("shell") && e.contains("调用模型前停止"), "{e}");
    }

    #[test]
    fn 多出别的mcp服务必须拒绝() {
        let n = ["mcp__device__observe".to_string(), "mcp__device__tap".to_string(), "mcp__fs__read".to_string()];
        assert!(check_names(&n, &allowed()).is_err());
    }

    #[test]
    fn 少了工具也要拒绝() {
        assert!(check_names(&["mcp__device__observe".to_string()], &allowed()).is_err(), "声明不全说明配置没生效，不能放行");
    }

    #[test]
    fn 资源协议自带的三个工具不算越界() {
        let n = ["mcp__device__observe", "mcp__device__tap", "list_mcp_resources", "read_mcp_resource", "list_mcp_resource_templates"].map(String::from);
        assert!(check_names(&n, &allowed()).is_ok());
    }
}
