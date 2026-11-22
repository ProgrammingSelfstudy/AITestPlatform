use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{bail, Context};
use server::{app, AppState, Config};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

/// `server import-cases --dir <目录> [--project <名字，默认 hafa>] [--block 编号=原因]...`
/// 本机管理操作，不开放成 HTTP 接口。全有或全无：任何文件有问题就一个都不导入。
async fn import_cases(args: &[String]) -> anyhow::Result<()> {
    let (mut dir, mut project, mut blocked) = (None, "hafa".to_string(), std::collections::BTreeMap::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().with_context(|| format!("{name} 后面要跟一个值"));
        match a.as_str() {
            "--dir" => dir = Some(val("--dir")?),
            "--project" => project = val("--project")?,
            "--block" => {
                let v = val("--block")?;
                let (code, why) = v.split_once('=').with_context(|| format!("--block 的格式是 编号=原因，收到 {v:?}"))?;
                blocked.insert(code.trim().to_string(), why.trim().to_string());
            }
            other => bail!("不认识的参数 {other}。用法：server import-cases --dir <目录> [--project <名字>] [--block 编号=原因]..."),
        }
    }
    let dir = dir.context("缺少 --dir <用例目录>")?;
    let cases = match server::cases::parse_dir(std::path::Path::new(&dir)) {
        Ok(c) => c,
        Err(errs) => {
            for e in &errs {
                eprintln!("✗ {e}");
            }
            bail!("{} 个问题，一个用例都没有导入", errs.len());
        }
    };
    let database_url = std::env::var("DATABASE_URL").context("请设置 DATABASE_URL（见仓库根目录 .env）")?;
    let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await.context("连接数据库失败")?;
    server::migrate(&pool).await.context("跑迁移失败")?;
    let r = server::cases::import(&pool, &project, &cases, &blocked).await.map_err(anyhow::Error::msg)?;
    println!("项目 {project}：解析 {} 个用例", cases.len());
    for (name, v) in [("新增", &r.inserted), ("内容变化（版本 +1）", &r.updated), ("状态变化", &r.status_changed)] {
        if !v.is_empty() {
            println!("  {name} {}：{}", v.len(), v.join("、"));
        }
    }
    println!("  无变化 {}", r.unchanged.len());
    Ok(())
}

/// 本机管理操作。错误只说明失败位置或类别，避免打印 YAML 内容、文件名和数据库诊断里的值。
async fn import_accounts(args: &[String]) -> anyhow::Result<()> {
    let (mut file, mut project, mut env, mut device) = (None, "hafa", "debug", None);
    let mut args = args.iter().map(String::as_str);
    while let Some(flag) = args.next() {
        let value = args.next().filter(|value| !value.starts_with("--")).context("导入参数缺少值")?;
        match flag {
            "--file" => file = Some(value),
            "--project" => project = value,
            "--env" => env = value,
            "--device" => device = Some(value),
            _ => bail!("用法：server import-accounts --file <文件> [--project hafa] [--env debug] [--device <序列号>]"),
        }
    }
    let file = file.context("缺少 --file <账号 YAML 文件>")?;
    let key = server::accounts::AccountKey::from_env()?;
    let yaml = std::fs::read_to_string(file).map_err(|_| anyhow::anyhow!("无法读取账号 YAML 文件"))?;
    let mut accounts = server::accounts::parse_accounts(&yaml, env)?;
    drop(yaml);
    if let Some(device) = device {
        for account in &mut accounts {
            account.device_serial.get_or_insert_with(|| device.to_owned());
        }
    }
    let database_url = std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("请设置 DATABASE_URL"))?;
    let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await.map_err(|_| anyhow::anyhow!("连接数据库失败"))?;
    server::migrate(&pool).await.map_err(|_| anyhow::anyhow!("数据库迁移失败"))?;
    let imported = server::accounts::import_accounts(&pool, &key, project, env, &accounts).await?;
    println!("已导入 {imported} 个账号");
    Ok(())
}

/// `server create-task --node <node_id> --cases TC-04,TC-05 [--project hafa] [--executor-model M] [--effort E] [--judge claude_cli]`
/// 本机管理操作，不开放成 HTTP 接口（没有鉴权前不能开放）。run 创建时就绑定到这个节点。
async fn create_task(args: &[String]) -> anyhow::Result<()> {
    use protocol::{ModelSource, RoleModel};
    let (mut node, mut cases, mut project, mut model, mut effort, mut judge_model, mut user) = (None, None, "hafa".to_string(), None, None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().with_context(|| format!("{name} 后面要跟一个值"));
        match a.as_str() {
            "--node" => node = Some(val("--node")?),
            "--cases" => cases = Some(val("--cases")?),
            "--project" => project = val("--project")?,
            "--executor-model" => model = Some(val("--executor-model")?),
            "--effort" => effort = Some(val("--effort")?),
            "--judge-model" => judge_model = Some(val("--judge-model")?),
            "--user" => user = Some(val("--user")?),
            other => bail!("不认识的参数 {other}。用法：server create-task --user <用户名> --node <id> --cases TC-04,TC-05 [--project 名] [--executor-model 模型] [--effort 强度]"),
        }
    }
    let node_id: uuid::Uuid = node.context("缺少 --node")?.parse().context("--node 不是合法的 id")?;
    let case_codes: Vec<String> = cases.context("缺少 --cases")?.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
    let database_url = std::env::var("DATABASE_URL").context("请设置 DATABASE_URL")?;
    let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await.context("连接数据库失败")?;
    server::migrate(&pool).await.context("跑迁移失败")?;
    // 规则 9：任务只能发到发起人自己的节点
    let user_name = user.context("缺少 --user <用户名>（任务只能发到发起人自己的节点）")?;
    let user_id = server::users::user_by_name(&pool, &user_name).await?.with_context(|| format!("没有这个用户：{user_name}（先用 server create-user 创建）"))?;
    if server::users::check_node_owner(&pool, node_id, user_id).await? == server::users::NodeOwner::NotYours {
        bail!("节点 {node_id} 不属于用户 {user_name}，不能用它的节点发起任务");
    }
    let t = server::runs::create_task(
        &pool,
        &server::runs::NewTask {
            project,
            node_id,
            case_codes,
            executor: RoleModel { source: ModelSource::CodexExec, model, effort },
            judge: RoleModel { source: ModelSource::ClaudeCli, model: judge_model, effort: None },
        },
    )
    .await?;
    println!("任务 {} 已创建，{} 个 run：{:?}", t.task_id, t.run_ids.len(), t.run_ids);
    Ok(())
}

/// `server create-user --name <名字>`：创建用户并打印个人令牌（只显示这一次，库里只存摘要）。
/// 用户把令牌填到节点的 `NODE_USER_TOKEN` 和前端发起任务的表单里。
async fn create_user(args: &[String]) -> anyhow::Result<()> {
    let name = match args {
        [flag, name] if flag == "--name" && !name.trim().is_empty() => name.trim(),
        _ => bail!("用法：server create-user --name <名字>"),
    };
    let database_url = std::env::var("DATABASE_URL").context("请设置 DATABASE_URL")?;
    let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await.map_err(|_| anyhow::anyhow!("连接数据库失败"))?;
    server::migrate(&pool).await.map_err(|_| anyhow::anyhow!("数据库迁移失败"))?;
    let token = server::users::create_user(&pool, name).await.map_err(|_| anyhow::anyhow!("创建用户失败（名字可能已存在）"))?;
    println!("用户 {name} 已创建。个人令牌（只显示这一次，请妥善保存）：\n{token}");
    Ok(())
}

async fn prune_recordings(args: &[String]) -> anyhow::Result<()> {
    use server::recording_retention::{DEFAULT_KEEP_DAYS, DEFAULT_MAX_BYTES, prune};
    let (mut keep_days, mut max_bytes, mut apply) = (DEFAULT_KEEP_DAYS, DEFAULT_MAX_BYTES, false);
    let mut args = args.iter().map(String::as_str);
    while let Some(flag) = args.next() {
        match flag {
            "--keep-days" => keep_days = args.next().context("缺少留存天数")?.parse().context("留存天数必须是整数")?,
            "--max-bytes" => max_bytes = args.next().context("缺少容量上限")?.parse().context("容量上限必须是非负整数字节数")?,
            "--apply" => apply = true,
            "--help" => {
                println!("用法：server prune-recordings [--keep-days 14] [--max-bytes 5368709120] [--apply]\n默认只预览，不删除录屏。使用与服务进程一致的 DATABASE_URL 和 PLATFORM_DATA_DIR。");
                return Ok(());
            }
            _ => bail!("不认识的参数；请运行 server prune-recordings --help"),
        }
    }
    let database_url = std::env::var("DATABASE_URL").context("请设置 DATABASE_URL")?;
    let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await.map_err(|_| anyhow::anyhow!("连接数据库失败"))?;
    let data = std::env::var_os("PLATFORM_DATA_DIR").map_or_else(|| "data".into(), std::path::PathBuf::from);
    let report = prune(&pool, &data, keep_days, max_bytes, apply).await?;
    let action = if apply { "已清理" } else { "预览：预计清理" };
    println!("{action} {} 个 run 的 {} 段录屏，共 {} 字节；清理后登记约 {} 字节", report.runs, report.segments, report.removed_bytes, report.remaining_bytes);
    if report.remaining_bytes > max_bytes {
        println!("容量仍超上限；不会清理运行中或仍在补传窗口内的录屏");
    }
    Ok(())
}

/// 监听地址：默认只允许 127.0.0.1。`allow_lan` 为真时允许私有网段 IP 或 0.0.0.0（监听所有网卡）。
/// 公网 IP 一律拒绝：没有鉴权，不能直接对公网。
fn checked_bind(value: &str, allow_lan: bool) -> anyhow::Result<SocketAddr> {
    let bind: SocketAddr = value.parse().context("PLATFORM_BIND 必须是 IP:端口，例如 127.0.0.1:8080")?;
    let ip = match bind.ip() {
        std::net::IpAddr::V4(ip) => ip,
        std::net::IpAddr::V6(_) => bail!("只支持 IPv4 监听地址"),
    };
    if ip == Ipv4Addr::LOCALHOST {
        return Ok(bind);
    }
    if !allow_lan {
        bail!("默认只允许监听 127.0.0.1。要放开局域网请同时设置 PLATFORM_ALLOW_LAN=1（注意：目前没有登录鉴权）");
    }
    if !(ip.is_unspecified() || ip.is_private()) {
        bail!("局域网模式只允许监听私有网段 IP 或 0.0.0.0，不能监听公网地址");
    }
    Ok(bind)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 仓库根目录的 .env（已被 git 忽略）里放 DATABASE_URL；环境变量优先
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("import-cases") {
        return import_cases(&args[1..]).await;
    }

    if args.first().map(String::as_str) == Some("import-accounts") {
        return import_accounts(&args[1..]).await;
    }

    if args.first().map(String::as_str) == Some("create-task") {
        return create_task(&args[1..]).await;
    }

    if args.first().map(String::as_str) == Some("create-user") {
        return create_user(&args[1..]).await;
    }

    if args.first().map(String::as_str) == Some("prune-recordings") {
        return prune_recordings(&args[1..]).await;
    }

    // 节点令牌必须显式给出，代码里不写默认值
    let node_token = std::env::var("NODE_TOKEN").unwrap_or_default();
    if node_token.is_empty() {
        bail!("请先设置环境变量 NODE_TOKEN（节点接入令牌），例如：NODE_TOKEN=$(openssl rand -hex 16) cargo run -p server");
    }
    let database_url = std::env::var("DATABASE_URL").context("请设置 DATABASE_URL（见仓库根目录 .env）")?;
    // 用户接口还没有登录鉴权，节点通过本机接口领取凭证。
    let allow_lan = std::env::var("PLATFORM_ALLOW_LAN").is_ok_and(|v| v == "1");
    let bind = checked_bind(&std::env::var("PLATFORM_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into()), allow_lan)?;
    if allow_lan {
        tracing::warn!("局域网模式已开启：查看无需口令，创建任务、取消执行和提交审核需要共享口令。只在可信网络里使用");
    }

    // 报错里不带连接串，避免把密码带进日志
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .context("连接数据库失败（检查 PostgreSQL 是否已启动、DATABASE_URL 是否正确）")?;
    let state = AppState::init(
        Config { node_token, hello_timeout: Duration::from_secs(10), heartbeat_s: protocol::DEFAULT_HEARTBEAT_S, lost_grace: Duration::from_secs(120) },
        pool,
    )
    .await
    .context("初始化数据库失败（迁移或重置状态出错）")?;

    let listener = tokio::net::TcpListener::bind(&bind).await.with_context(|| format!("无法监听 {bind}"))?;
    tracing::info!("平台服务端已启动：http://{bind}");
    state.spawn_housekeeping(Duration::from_secs(30));
    axum::serve(listener, if allow_lan { server::app_lan(state) } else { app(state) }).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认只允许监听本机_局域网要显式开启且不能是公网() {
        assert!(checked_bind("127.0.0.1:8080", false).is_ok());
        assert!(checked_bind("0.0.0.0:8080", false).is_err(), "没开开关不能监听所有网卡");
        assert!(checked_bind("192.168.1.5:8080", false).is_err());
        assert!(checked_bind("[::1]:8080", false).is_err());
        assert!(checked_bind("0.0.0.0:8080", true).is_ok());
        assert!(checked_bind("192.168.1.5:8080", true).is_ok());
        assert!(checked_bind("10.0.0.2:8080", true).is_ok());
        assert!(checked_bind("8.8.8.8:8080", true).is_err(), "公网地址任何时候都不行");
        assert!(checked_bind("[::]:8080", true).is_err());
    }
}
