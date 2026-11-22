//! 整条链路的真实验证：`claude` 会员命令行 → Rust 设备 MCP 服务 → 真机。
//!
//! 只放行只读的 `observe`；`tap_element` 不在允许列表里，模型就算想点手机也会被拒绝，
//! 所以这个测试不会改变手机状态。会消耗一点会员额度，因此默认忽略：
//!
//!     cargo build -p mcp-device && cargo test -p agent --test chain -- --ignored --nocapture

use std::time::Duration;

use agent::claude_cli::{run, ClaudeOpts};

const MCP_BIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/debug/mcp-device");

#[tokio::test]
#[ignore]
async fn 真实链路_claude会员到rust_mcp到真机() {
    assert!(std::path::Path::new(MCP_BIN).exists(), "先运行 cargo build -p mcp-device");
    let devices = device::list_devices().await.unwrap();
    let serial = devices.iter().find(|d| d.state == "device").expect("没有在线设备").serial.clone();

    // 本次执行的轨迹目录：设备服务没有它就拒绝启动
    let run_dir = std::env::temp_dir().join(format!("chain_run_{}", std::process::id()));
    // MCP 配置：告诉 claude 命令行怎么拉起我们的设备服务（序列号只经环境变量传，不写进命令行）
    let cfg = std::env::temp_dir().join(format!("mcp_chain_{}.json", std::process::id()));
    std::fs::write(
        &cfg,
        serde_json::json!({"mcpServers": {"device": {"command": MCP_BIN, "args": [], "env": {"HAFA_SERIAL": serial, "HAFA_RUN_DIR": run_dir}}}}).to_string(),
    )
    .unwrap();

    let opts = ClaudeOpts {
        mcp_config: Some(cfg.clone()),
        allowed_tools: vec!["mcp__device__observe".into()], // 只放行只读工具
        stall_timeout: Duration::from_secs(120),
        total_timeout: Duration::from_secs(240),
        ..Default::default()
    };
    let r = run(&opts, "调用一次 device 的 observe 工具读取手机当前页面，然后用一句话说明页面上能看到什么。只调用 observe 一次，不要点击任何东西。").await;
    let _ = std::fs::remove_file(&cfg);

    println!("失败：{:?}", r.failure);
    println!("工具调用：{:?}", r.tool_calls);
    println!("回答：{}", r.text);
    println!("折算费用：{:?}  额度：{:?}", r.cost_usd, r.rate_limit);

    assert!(r.ok(), "{:?}", r.failure);
    assert!(r.tool_calls.iter().any(|t| t == "mcp__device__observe"), "模型必须真的调了 observe：{:?}", r.tool_calls);
    assert!(!r.tool_calls.iter().any(|t| t.contains("tap")), "不应出现点击：{:?}", r.tool_calls);
    assert!(!r.text.trim().is_empty());
}
