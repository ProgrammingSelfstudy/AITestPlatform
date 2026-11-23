use std::path::PathBuf;

use anyhow::{bail, Context};
use node::real::{RealConfig, RealExecutor};
use node::{run, AdbDevices, NoExecutor, NodeConfig, Only};
use reqwest::Url;
use tracing_subscriber::EnvFilter;

/// `node gen-script <run编号> [--app-info <app_info.yaml>]`：为本节点上一次通过的执行生成回归脚本候选并上传归档。
/// 显式触发而不是每次通过都自动跑：它会花发起人的 Claude 会员额度（规则 9），由人决定哪次值得固化。
async fn gen_script(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str = "用法：node gen-script <run编号> [--app-info <app_info.yaml>] [--fix-from <被拒绝的脚本编号>]";
    let (run, mut rest) = args.split_first().context(USAGE)?;
    let run_id: uuid::Uuid = run.parse().context("run 编号必须是 UUID")?;
    let mut fix_from = None;
    if let [head @ .., flag, id] = rest && flag == "--fix-from" {
        fix_from = Some(id.parse::<uuid::Uuid>().context("--fix-from 的脚本编号必须是 UUID")?);
        rest = head;
    }
    let ui_map = match rest {
        [] => serde_json::Value::Null,
        [flag, file] if flag == "--app-info" => {
            let text = std::fs::read_to_string(file).with_context(|| format!("无法读取应用知识 {file}"))?;
            let yaml: serde_json::Value = serde_yaml_ng::from_str(&text).context("应用知识不是有效的 YAML")?;
            yaml.get("ui_map").cloned().unwrap_or(serde_json::Value::Null)
        }
        _ => bail!("{USAGE}"),
    };
    let token = std::env::var("NODE_TOKEN").unwrap_or_default();
    if token.is_empty() {
        bail!("请先设置环境变量 NODE_TOKEN（和服务端一致的节点接入令牌）");
    }
    let ws = std::env::var("PLATFORM_WS").unwrap_or_else(|_| "ws://127.0.0.1:8080/ws/node".into());
    let state_dir = PathBuf::from(std::env::var("NODE_STATE_DIR").unwrap_or_else(|_| "data/node".into()));
    let node_id = node::load_node_id(&state_dir).context("没有找到本节点的 node_id：节点还没有连过服务端，或 NODE_STATE_DIR 不对")?;
    node::load_node_secret(&state_dir);
    let cfg = node::scriptgen_run::GenConfig {
        platform_http: platform_http(&ws, std::env::var("NODE_ALLOW_INSECURE").is_ok_and(|v| v == "1"))?,
        node_token: token,
        node_id,
        runs_dir: std::env::var("HAFA_RUNS_DIR").unwrap_or_else(|_| "data/runs".into()).into(),
        claude: agent::claude_cli::ClaudeOpts::default(),
        ui_map,
        fix_from,
    };
    match node::scriptgen_run::generate_and_upload(&cfg, run_id).await? {
        node::scriptgen_run::Outcome::Created { id, version } => {
            println!("已归档为版本 {version}（脚本 {id}，状态：候选）。候选还要自检通过才能获准，目前还没有自动自检");
        }
        node::scriptgen_run::Outcome::Existing { id, version } => {
            println!("同样内容之前已经归档为版本 {version}（脚本 {id}），没有新增");
        }
    }
    Ok(())
}

/// 自检和回归共用的配置：真实执行配置加上要用的那台设备。
/// 序列号取 `--serial`，没给就取 `HAFA_DEVICE_ALLOW`（只有一台时）。
fn check_config(serial: Option<String>) -> anyhow::Result<node::selfcheck_run::CheckConfig> {
    let allow: Vec<String> = std::env::var("HAFA_DEVICE_ALLOW").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let serial = match serial {
        Some(serial) => serial,
        None if allow.len() == 1 => allow[0].clone(),
        None => bail!("没有指定设备：加 --serial，或让 HAFA_DEVICE_ALLOW 只含一台设备"),
    };
    let token = std::env::var("NODE_TOKEN").unwrap_or_default();
    if token.is_empty() {
        bail!("请先设置环境变量 NODE_TOKEN（和服务端一致的节点接入令牌）");
    }
    let ws = std::env::var("PLATFORM_WS").unwrap_or_else(|_| "ws://127.0.0.1:8080/ws/node".into());
    let state_dir = PathBuf::from(std::env::var("NODE_STATE_DIR").unwrap_or_else(|_| "data/node".into()));
    let node_id = node::load_node_id(&state_dir).context("没有找到本节点的 node_id：节点还没有连过服务端，或 NODE_STATE_DIR 不对")?;
    node::load_node_secret(&state_dir);
    let real = real_config(&NodeConfig::new(ws, token, state_dir))?.context("需要 HAFA_PYTHON 等真实执行配置")?;
    Ok(node::selfcheck_run::CheckConfig {
        platform_http: real.platform_http.clone(),
        node_token: real.node_token.clone(),
        node_id,
        python: real.python.clone(),
        automation_dir: real.automation_dir.clone(),
        audio_dir: real.audio_dir.clone(),
        state_dir: real.state_dir.clone(),
        package: real.package.clone(),
        activity: real.activity.clone(),
        bottom_shift: real.bottom_shift.get(&serial).copied().unwrap_or(0),
        serial,
        timeout: std::time::Duration::from_secs(15 * 60),
        work_dir: std::env::temp_dir().join("hafa_selfcheck"),
    })
}

/// `node self-check <脚本编号> [--serial <设备序列号>]`：在本节点的真机上自检一条候选脚本。
/// 不花模型额度，但会占用这台手机：先确认节点没有正在这台手机上跑任务。
/// 没有账号租约，手机上要已经登录好目标账号。
async fn self_check(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str = "用法：node self-check <脚本编号> [--serial <设备序列号>]";
    let (script, rest) = args.split_first().context(USAGE)?;
    let script_id: uuid::Uuid = script.parse().context("脚本编号必须是 UUID")?;
    let serial = match rest {
        [flag, serial] if flag == "--serial" => Some(serial.clone()),
        [] => None,
        _ => bail!("{USAGE}"),
    };
    let cfg = check_config(serial)?;
    println!("开始在设备 {} 上自检，会占用这台手机，请确认节点没有在它上面跑任务", cfg.serial);
    let (outcome, detail) = node::selfcheck_run::self_check(&cfg, script_id).await?;
    match outcome {
        node::selfcheck_run::Outcome::Passed => println!("自检通过：{detail}\n脚本状态已改为「自检通过」，获准要由人来决定"),
        node::selfcheck_run::Outcome::Rejected => println!("自检失败，脚本已标记为「已拒绝」。原始输出：\n{detail}"),
    }
    Ok(())
}

/// `node heal <脚本编号> [--serial <设备序列号>] [--app-info <app_info.yaml>]`：已获准脚本回归失败后做自愈分诊。
/// 会花发起人的 Claude 会员额度，也会占用这台手机重跑一遍脚本取新证据，所以显式触发。
async fn heal(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str = "用法：node heal <脚本编号> [--serial <设备序列号>] [--app-info <app_info.yaml>]";
    let (script, mut rest) = args.split_first().context(USAGE)?;
    let script_id: uuid::Uuid = script.parse().context("脚本编号必须是 UUID")?;
    let (mut serial, mut ui_map) = (None, serde_json::Value::Null);
    while let [flag, value, tail @ ..] = rest {
        match flag.as_str() {
            "--serial" => serial = Some(value.clone()),
            "--app-info" => {
                let text = std::fs::read_to_string(value).with_context(|| format!("无法读取应用知识 {value}"))?;
                let yaml: serde_json::Value = serde_yaml_ng::from_str(&text).context("应用知识不是有效的 YAML")?;
                ui_map = yaml.get("ui_map").cloned().unwrap_or(serde_json::Value::Null);
            }
            _ => bail!("{USAGE}"),
        }
        rest = tail;
    }
    if !rest.is_empty() {
        bail!("{USAGE}");
    }
    let cfg = node::heal_run::HealConfig {
        check: check_config(serial)?,
        runs_dir: std::env::var("HAFA_RUNS_DIR").unwrap_or_else(|_| "data/runs".into()).into(),
        claude: agent::claude_cli::ClaudeOpts::default(),
        ui_map,
    };
    println!("开始自愈分诊脚本 {script_id}：会在设备 {} 上重跑一遍取新证据，并调用 Claude 会员命令行", cfg.check.serial);
    let report = node::heal_run::heal_script(&cfg, script_id).await?;
    match (report.verdict, report.healed) {
        (agent::scriptgen::HealVerdict::LocatorBroken, Some((id, version))) => {
            println!("定位失效：已归档修复候选版本 {version}（脚本 {id}）。原获准版本没有动；候选要自检通过、再由人获准（先下线旧版本）才会被回归使用。\n依据：{}", report.reason);
        }
        (agent::scriptgen::HealVerdict::BusinessBug, _) => println!("业务行为错误：脚本没有改，失败的回归记录保留，请按缺陷处理。\n依据：{}", report.reason),
        _ => println!("分不清，交人工判断：脚本没有改，失败的回归记录保留。\n原因：{}", report.reason),
    }
    Ok(())
}

/// `node regress <用例编号> [--project <项目>] [--serial <设备序列号>]`：用该用例当前已获准的脚本版本在本节点真机上回归，
/// 结果记到平台。失败是回归的结论，如实记录，不改脚本状态。会占用这台手机。
async fn regress(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str = "用法：node regress <用例编号> [--project <项目，默认 hafa>] [--serial <设备序列号>]";
    let (case_code, mut rest) = args.split_first().context(USAGE)?;
    let (mut project, mut serial) = ("hafa".to_string(), None);
    while let [flag, value, tail @ ..] = rest {
        match flag.as_str() {
            "--project" => project = value.clone(),
            "--serial" => serial = Some(value.clone()),
            _ => bail!("{USAGE}"),
        }
        rest = tail;
    }
    if !rest.is_empty() {
        bail!("{USAGE}");
    }
    let cfg = check_config(serial)?;
    println!("开始在设备 {} 上回归 {project} 的 {case_code}，会占用这台手机，请确认节点没有在它上面跑任务", cfg.serial);
    let outcome = node::regress_run::regress(&cfg, &project, case_code).await?;
    if outcome.passed {
        println!("回归通过（脚本版本 {}）：{}", outcome.version, outcome.detail);
    } else {
        println!("回归失败（脚本版本 {}），已如实记录。可能是产品退化，也可能是脚本随界面改版失效，需要人判断。原始输出：\n{}", outcome.version, outcome.detail);
        std::process::exit(1);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("gen-script") {
        return gen_script(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("self-check") {
        return self_check(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("heal") {
        return heal(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("regress") {
        return regress(&args[1..]).await;
    }

    // 令牌必须显式给出；不写日志
    let token = std::env::var("NODE_TOKEN").unwrap_or_default();
    if token.is_empty() {
        bail!("请先设置环境变量 NODE_TOKEN（和服务端一致的节点接入令牌）");
    }
    let url = std::env::var("PLATFORM_WS").unwrap_or_else(|_| "ws://127.0.0.1:8080/ws/node".into());
    let state_dir = PathBuf::from(std::env::var("NODE_STATE_DIR").unwrap_or_else(|_| "data/node".into()));

    tracing::info!("执行节点启动，连接 {url}");
    let mut cfg = NodeConfig::new(url, token, state_dir);
    cfg.user_token = std::env::var("NODE_USER_TOKEN").ok().filter(|v| !v.is_empty());
    if cfg.user_token.is_none() {
        tracing::warn!("没有设置 NODE_USER_TOKEN：节点没有属主，任何人都发不了任务到这个节点（先 server create-user，再把令牌设进来）");
    }
    // HAFA_DEVICE_ALLOW=序列号,序列号：只让这些设备参与（账号不能同时登在两台手机上时用）
    let allow: Vec<String> = std::env::var("HAFA_DEVICE_ALLOW").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if !allow.is_empty() {
        tracing::info!("设备白名单：{allow:?}");
    }
    // 配了 HAFA_PYTHON 就具备真实执行能力；否则领到任务只会如实报受阻
    match real_config(&cfg)? {
        Some(rc) => {
            tracing::info!("真实执行器已启用");
            // 只有一台确定的设备时才接页面发起的回归（多台设备要显式指定用哪台：`node regress --serial`）
            if allow.len() == 1 {
                // node_id 要等节点第一次连上服务端才有，所以配置拿不到时隔几秒重试，而不是放弃
                tokio::spawn(async {
                    let check = loop {
                        match check_config(None) {
                            Ok(check) => break check,
                            Err(error) => {
                                tracing::info!("暂时不能接收页面发起的回归，稍后重试：{error:#}");
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            }
                        }
                    };
                    tracing::info!("接收页面发起的回归，使用设备 {}", check.serial);
                    node::regress_poll::serve(check, std::time::Duration::from_secs(5)).await;
                });
            } else {
                tracing::info!("HAFA_DEVICE_ALLOW 不是恰好一台设备，不接收页面发起的回归");
            }
            tokio::select! {
                r = run(cfg, Only { inner: AdbDevices::default(), serials: allow }, RealExecutor(rc)) => r?,
                _ = tokio::signal::ctrl_c() => tracing::info!("收到退出信号，节点停止"),
            }
        }
        None => {
            tracing::warn!("没有配置 HAFA_PYTHON，节点没有执行能力");
            tokio::select! {
                r = run(cfg, Only { inner: AdbDevices::default(), serials: allow }, NoExecutor) => r?,
                _ = tokio::signal::ctrl_c() => tracing::info!("收到退出信号，节点停止"),
            }
        }
    }
    Ok(())
}

/// 是否回环地址（本机直连，明文不出机器）。
fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 由节点连接地址推导平台 HTTP 基址。`wss://` → `https://`。
/// 明文 `ws://` 只允许回环地址：令牌、节点密钥和账号明文都走这条链路，跨机必须加密；
/// 确有内网明文需求时显式设 `NODE_ALLOW_INSECURE=1`。
fn platform_http(ws: &str, allow_insecure: bool) -> anyhow::Result<Url> {
    let mut url = Url::parse(ws).context("PLATFORM_WS 不是合法的地址")?;
    if !matches!(url.scheme(), "ws" | "wss") || url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        bail!("PLATFORM_WS 必须是无用户信息的 ws:// 或 wss:// 地址");
    }
    if url.scheme() == "ws" && !is_loopback(&url) && !allow_insecure {
        bail!("PLATFORM_WS 不是本机地址，必须用 wss://（令牌和账号凭证会经过这条连接）；内网确需明文请设置 NODE_ALLOW_INSECURE=1");
    }
    let http = if url.scheme() == "wss" { "https" } else { "http" };
    url.set_scheme(http).map_err(|_| anyhow::anyhow!("无法推导平台 HTTP 地址"))?;
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// 环境变量：HAFA_PYTHON、HAFA_AUTOMATION_DIR、HAFA_AUDIO_DIR、HAFA_MCP_BIN、HAFA_APP_INFO（app_info.yaml 路径）、
/// HAFA_RUNS_DIR、HAFA_BOTTOM_SHIFTS（如 `15064060=-84,其它=0`）、HAFA_EVIDENCE_BIN（配置后启用独立判定）。
fn real_config(node: &NodeConfig) -> anyhow::Result<Option<RealConfig>> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let Some(python) = get("HAFA_PYTHON") else { return Ok(None) };
    let need = |k: &str| get(k).ok_or_else(|| anyhow::anyhow!("配置了 HAFA_PYTHON 就必须同时设置 {k}"));
    let app_info = std::fs::read_to_string(need("HAFA_APP_INFO")?)?;
    let mut shifts = std::collections::HashMap::new();
    for kv in get("HAFA_BOTTOM_SHIFTS").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("HAFA_BOTTOM_SHIFTS 格式是 序列号=偏移,..."))?;
        shifts.insert(k.trim().to_string(), v.trim().parse()?);
    }
    Ok(Some(RealConfig {
        platform_http: platform_http(&node.url, std::env::var("NODE_ALLOW_INSECURE").is_ok_and(|v| v == "1"))?,
        node_token: node.token.clone(),
        python,
        automation_dir: need("HAFA_AUTOMATION_DIR")?.into(),
        audio_dir: need("HAFA_AUDIO_DIR")?.into(),
        state_dir: std::env::temp_dir().join("hafa_node_state"),
        runs_dir: get("HAFA_RUNS_DIR").unwrap_or_else(|| "data/runs".into()).into(),
        mcp_bin: need("HAFA_MCP_BIN")?.into(),
        evidence_bin: get("HAFA_EVIDENCE_BIN").unwrap_or_default().into(),
        package: "com.hafa.pikka".into(),
        activity: "com.hafa.pikka.MainActivity".into(),
        bottom_shift: shifts,
        screen: (720, 1600),
        app_info,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 从节点连接地址推导平台_http_基址() {
        let base = platform_http("ws://127.0.0.1:9000/ws/node?param=value", false).unwrap();
        assert_eq!(base.as_str(), "http://127.0.0.1:9000/");
    }

    #[test]
    fn wss_推导为_https() {
        assert_eq!(platform_http("wss://ci.example.com/ws/node", false).unwrap().as_str(), "https://ci.example.com/");
    }

    #[test]
    fn 跨机明文被拒绝_本机和显式放行除外() {
        assert!(platform_http("ws://192.168.1.5:8080/ws/node", false).is_err());
        assert!(platform_http("ws://ci.example.com/ws/node", false).is_err());
        assert!(platform_http("ws://localhost:8080/ws/node", false).is_ok());
        assert!(platform_http("ws://[::1]:8080/ws/node", false).is_ok());
        assert!(platform_http("ws://192.168.1.5:8080/ws/node", true).is_ok(), "显式放行");
        assert!(platform_http("http://127.0.0.1/ws/node", false).is_err());
    }
}
