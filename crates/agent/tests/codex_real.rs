//! 真实 `codex` 的本地审计：模型端点指到 127.0.0.1，不带凭证，**不消耗 GPT 额度**。
//! 验证本机 codex 接受锁死配置（`--strict-config` 会拒绝不认识的项），且声明的工具恰好等于设备工具白名单。
//!
//!     cargo build -p mcp-device && cargo test -p agent --test codex_real -- --ignored --nocapture

use agent::codex_exec::audit::audit_tools;
use agent::codex_exec::command::{build_args, catalog, filter_env, Bin, DeviceMcp, Spec};

const MCP_BIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/debug/mcp-device");
use agent::prompts::DEVICE_TOOLS as TOOLS;

#[tokio::test]
#[ignore]
async fn 真实codex_工具清单恰好等于设备工具白名单() {
    assert!(std::path::Path::new(MCP_BIN).exists(), "先运行 cargo build -p mcp-device");
    let work = std::env::temp_dir().join(format!("codex_real_{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let env = filter_env(std::env::vars());
    let path_env = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone()).unwrap_or_default();

    let bin = Bin::default();
    let (cat, model) = catalog(&bin, &work, None, &env).await.expect("读取本机 codex 模型目录失败");
    println!("使用模型目录里的：{model}");
    let instr = work.join("instructions.txt");
    std::fs::write(&instr, "审计用，不会调用模型").unwrap();

    let mcp = DeviceMcp {
        command: MCP_BIN.into(),
        env: vec![
            ("HAFA_SERIAL".into(), "audit-no-device".into()),
            ("HAFA_RUN_DIR".into(), work.join("run").to_string_lossy().into()),
        ],
    };
    let allowed: Vec<String> = TOOLS.map(String::from).into();
    let spec = Spec { model: &model, work: &work, catalog: &cat, instructions: &instr, effort: None, mcp: &mcp, allowed: &allowed, path_env: &path_env };
    let names = audit_tools(&bin, &build_args(&spec, None), &allowed, &work, &path_env).await.expect("审计失败");
    println!("codex 实际声明的工具：{names:#?}");
    // 资源协议自带的三个工具审计里本来就允许，其余必须全是设备工具，且恰好是白名单里的 11 个
    let resource = ["list_mcp_resource_templates", "list_mcp_resources", "read_mcp_resource"];
    let device: Vec<&String> = names.iter().filter(|n| !resource.contains(&n.as_str())).collect();
    assert_eq!(device.len(), TOOLS.len(), "设备工具数量不对：{names:?}");
    assert!(device.iter().all(|n| n.starts_with("mcp__device.")), "声明里不能有设备工具以外的东西：{names:?}");
    assert!(!names.iter().any(|n| n.contains("shell") || n.contains("apply_patch") || n.contains("web_search")), "不能有 shell / 改文件 / 联网搜索：{names:?}");
}
