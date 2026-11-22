//! 设备 MCP 服务的协议测试：把编译出来的二进制当子进程拉起，用 rmcp 客户端经 stdio 对话。

use rmcp::model::CallToolRequestParams;
use rmcp::serde_json;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_mcp-device");

fn run_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!("mcp_stdio_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
}

async fn connect(serial: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    connect_in(serial, run_dir()).await
}

async fn connect_in(serial: &str, dir: std::path::PathBuf) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let serial = serial.to_string();
    let transport = TokioChildProcess::new(Command::new(BIN).configure(move |c| {
        c.env("HAFA_SERIAL", serial).env("HAFA_RUN_DIR", dir);
    }))
    .expect("拉起 mcp-device 失败");
    ().serve(transport).await.expect("MCP 握手失败")
}

#[tokio::test]
async fn 列出工具和参数模式() {
    let client = connect("不会被用到").await;
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    // 提示词里教模型用的工具（DEVICE_TOOLS）必须恰好等于服务实际暴露的工具：
    // 多一个，模型会去调用不存在的工具；少一个，模型有能力却不知道。
    let mut want: Vec<&str> = agent::prompts::DEVICE_TOOLS.to_vec();
    want.sort();
    assert_eq!(names, want, "设备服务暴露的工具和提示词里的工具清单不一致");

    let tap = tools.iter().find(|t| t.name == "tap").unwrap();
    let schema = serde_json::to_string(&tap.input_schema).unwrap();
    assert!(schema.contains("element_id"), "tap 的参数模式里应有 element_id：{schema}");
    assert!(!names.iter().any(|n| n == "input_text"), "文字输入还没做，不能假装有这个工具");
    assert!(tools.iter().all(|t| t.description.is_some()), "每个工具都要有描述，模型才知道怎么用");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn 设备命令失败时原始错误原样带出() {
    // 不存在的序列号：adb 会真实报错，错误必须带着序列号和 adb 的原始输出回到调用方
    let client = connect("不存在的序列号").await;
    let err = client.call_tool(CallToolRequestParams::new("observe")).await.unwrap_err().to_string();
    assert!(err.contains("设备命令失败"), "{err}");
    assert!(err.contains("不存在的序列号"), "错误里应带出命令和序列号：{err}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn 没有轨迹目录时拒绝启动() {
    let out = Command::new(BIN).env("HAFA_SERIAL", "x").env_remove("HAFA_RUN_DIR").output().await.unwrap();
    assert!(!out.status.success(), "没地方落轨迹就不能执行");
    assert!(String::from_utf8_lossy(&out.stderr).contains("HAFA_RUN_DIR"));
    assert!(out.stdout.is_empty(), "stdout 是协议通道，报错不能写到 stdout");
}

#[tokio::test]
async fn 没有设备序列号时拒绝启动() {
    let out = Command::new(BIN).env_remove("HAFA_SERIAL").output().await.unwrap();
    assert!(!out.status.success(), "没有 HAFA_SERIAL 不能启动：两台手机并行时选错设备会点错");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("HAFA_SERIAL"), "报错要说明缺什么：{stderr}");
    assert!(out.stdout.is_empty(), "stdout 是协议通道，报错不能写到 stdout");
}

/// 真机测试：`cargo test -p mcp-device -- --ignored --nocapture`。只读：observe，以及点一个不存在的元素。
#[tokio::test]
#[ignore]
async fn 真机_observe返回文字和截图() {
    let devices = device::list_devices().await.unwrap();
    let d = devices.iter().find(|d| d.state == "device").expect("没有在线设备");
    let dir = run_dir();
    let client = connect_in(&d.serial, dir.clone()).await;

    let r = client.call_tool(CallToolRequestParams::new("observe")).await.unwrap();
    assert_ne!(r.is_error, Some(true), "{r:?}");
    let text = r.content.iter().find_map(|c| c.as_text().map(|t| t.text.clone())).expect("应有文字内容");
    println!("observe 文字（前 300 字）：{}", text.chars().take(300).collect::<String>());
    let img = r.content.iter().find_map(|c| c.as_image().cloned()).expect("应有截图");
    assert_eq!(img.mime_type, "image/jpeg", "给模型的是缩小后的 JPEG");
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD.decode(&img.data).unwrap();
    assert_eq!(&png[..2], &[0xFF, 0xD8], "截图应是 JPEG");
    assert!(dir.join("shots/01_observe.png").exists(), "标注图要落盘");
    assert!(dir.join("trace.json").exists(), "一次执行从一开始就有 trace.json");

    // 点一个越界的编号：只返回提示，不会动设备
    let args = serde_json::json!({"element_id": 99999});
    let r = client
        .call_tool(CallToolRequestParams::new("tap").with_arguments(args.as_object().unwrap().clone()))
        .await
        .unwrap();
    let m = r.content.iter().find_map(|c| c.as_text().map(|t| t.text.clone())).unwrap();
    assert!(m.contains("element_id 越界"), "{m}");
    client.cancel().await.unwrap();
}
