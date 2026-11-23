//! 用例生成的端到端测试：进程内起真实服务端和数据库，模型调用用假的生成函数（不花额度、不碰真实命令行）。

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::casegen::Outcome;
use base64::Engine as _;
use node::casegen_run::{poll_once, CasegenConfig, Generate};
use node::roles::{real_probe, RoleStore};
use protocol::{ModelSource, RoleConfig, RoleDefault, RoleModelOption, RoleSet, RoleSlot};
use server::modules::{create_request, validate, CasegenInput, ShotInput};
use server::test_support::test_state;
use server::{app, AppState, Config};
use uuid::Uuid;

const TOKEN: &str = "casegen-run-token";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nxxxx";
const GOOD: &str = "id: G1\ntitle: 进入语聊房\nlevel: 简单流程\nplatform: [android]\nsteps:\n  - 点击语聊房入口\nexpected:\n  - 出现语聊房页面\n---\nid: G2\ntitle: 退出语聊房\nsteps:\n  - 点击退出\nexpected:\n  - 回到首页\n";

struct World {
    st: AppState,
    node: Uuid,
    user: Uuid,
    cfg: CasegenConfig,
    dir: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

async fn world() -> World {
    let st = test_state(Config { node_token: TOKEN.into(), hello_timeout: Duration::from_secs(1), heartbeat_s: 1, lost_grace: Duration::from_secs(120) }).await;
    let node = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, '{}'::jsonb, true)").bind(node).execute(st.pool()).await.unwrap();
    let user: Uuid = sqlx::query_scalar("insert into users (name, token_hash) values ($1, 'x') returning id").bind(format!("u-{node}")).fetch_one(st.pool()).await.unwrap();
    sqlx::query("insert into projects (name) values ('p')").execute(st.pool()).await.unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let router = app(st.clone());
    tokio::spawn(async move {
        let _ = axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), router).await;
    });
    let dir = std::env::temp_dir().join(format!("node_casegen_{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let cfg = CasegenConfig { platform_http: format!("http://{addr}/").parse().unwrap(), node_token: TOKEN.into(), node_id: node, state_dir: dir.clone() };
    World { st, node, user, cfg, dir }
}

fn writer_role(id: &str, source: ModelSource) -> RoleSet {
    RoleSet {
        config: RoleConfig {
            id: id.into(),
            slot: RoleSlot::CaseWriter,
            name: format!("测试用例·{id}"),
            source,
            default: RoleDefault { model: "角色指定的模型".into(), effort: Some("high".into()) },
            models: vec![RoleModelOption { model: "角色指定的模型".into(), efforts: vec!["high".into()] }],
        },
        api_key: None,
    }
}

fn enqueue_input(world: &World, module: &str, writer_role: Option<&str>) -> CasegenInput {
    CasegenInput {
        module: module.into(),
        doc: "# 需求\n用户可以进入语聊房。账号 {{account.username}}。".into(),
        ui_links: vec!["https://example.com/design/1".into()],
        screenshots: vec![ShotInput { data_b64: base64::engine::general_purpose::STANDARD.encode(PNG) }],
        node_id: world.node,
        writer_role: writer_role.map(str::to_owned),
        reviewer_role: None,
    }
}

async fn enqueue(world: &World, module: &str, writer_role: Option<&str>) -> Uuid {
    let input = enqueue_input(world, module, writer_role);
    let valid = validate(&input).unwrap();
    create_request(world.st.pool(), "p", world.user, &input, &valid).await.unwrap().request_id
}

#[derive(Default, Clone)]
struct Seen(Arc<Mutex<Vec<(ModelSource, Option<String>, Option<String>, String, usize, Vec<String>)>>>);

fn fake(seen: &Seen, outcome: impl Fn() -> Outcome + Send + Sync + 'static) -> Generate {
    let seen = seen.clone();
    Box::new(move |source, model, effort, input| {
        seen.0.lock().unwrap().push((source, model, effort, input.doc.to_string(), input.images.len(), input.ui_links.to_vec()));
        let outcome = outcome();
        Box::pin(async move { outcome })
    })
}

async fn status_of(world: &World, id: Uuid) -> (String, String) {
    sqlx::query_as("select status, detail from casegen_requests where id = $1").bind(id).fetch_one(world.st.pool()).await.unwrap()
}

#[tokio::test]
async fn 领取后按角色生成_用例以草稿入库() {
    let w = world().await;
    RoleStore::open(&w.dir, real_probe()).set(vec![writer_role("w1", ModelSource::ClaudeCli)]).unwrap();
    let id = enqueue(&w, "语聊房", None).await;
    let seen = Seen::default();
    let generate = fake(&seen, || Outcome { text: GOOD.into(), wall_s: 3.0, ..Default::default() });
    assert!(poll_once(&w.cfg, &generate).await.unwrap(), "应该领到并处理了一个请求");
    assert!(!poll_once(&w.cfg, &generate).await.unwrap(), "没有请求了");
    // 模型调用用的是角色配置里的来源、模型和强度；拿到的是文档、截图和链接，没有别的
    let calls = seen.0.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!((calls[0].0, calls[0].1.as_deref(), calls[0].2.as_deref()), (ModelSource::ClaudeCli, Some("角色指定的模型"), Some("high")));
    assert!(calls[0].3.contains("语聊房") && calls[0].4 == 1 && calls[0].5 == ["https://example.com/design/1"]);
    let (status, detail) = status_of(&w, id).await;
    assert_eq!(status, "finished", "{detail}");
    let rows: Vec<(String, String, String)> = sqlx::query_as("select code, origin, review_status from test_cases where module_id is not null order by code").fetch_all(w.st.pool()).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.0.starts_with("GEN-") && r.1 == "generated" && r.2 == "draft"), "{rows:?}");
}

#[tokio::test]
async fn 没有测试用例角色时如实报失败_不换别的角色() {
    let w = world().await;
    // 只配了一个执行员角色：不能拿来当测试用例角色
    let mut exec = writer_role("e", ModelSource::CodexExec);
    exec.config.slot = RoleSlot::Executor;
    RoleStore::open(&w.dir, real_probe()).set(vec![exec]).unwrap();
    let id = enqueue(&w, "语聊房", None).await;
    let seen = Seen::default();
    assert!(poll_once(&w.cfg, &fake(&seen, Outcome::default)).await.unwrap());
    assert!(seen.0.lock().unwrap().is_empty(), "没有角色时不能调模型");
    let (status, detail) = status_of(&w, id).await;
    assert_eq!(status, "failed");
    assert!(detail.contains("没有配置「测试用例」角色"), "{detail}");
}

#[tokio::test]
async fn 请求指定的角色不存在时不退回别的角色() {
    let w = world().await;
    RoleStore::open(&w.dir, real_probe()).set(vec![writer_role("w1", ModelSource::ClaudeCli)]).unwrap();
    let id = enqueue(&w, "语聊房", Some("不存在的角色")).await;
    let seen = Seen::default();
    assert!(poll_once(&w.cfg, &fake(&seen, Outcome::default)).await.unwrap());
    assert!(seen.0.lock().unwrap().is_empty());
    assert_eq!(status_of(&w, id).await.0, "failed");
}

#[tokio::test]
async fn 额度用完和模型失败如实回报_不入库() {
    let w = world().await;
    RoleStore::open(&w.dir, real_probe()).set(vec![writer_role("w1", ModelSource::CodexExec)]).unwrap();
    let id = enqueue(&w, "语聊房", None).await;
    let generate = fake(&Seen::default(), || Outcome { failure: Some("Codex 额度用完或被限流：limit".into()), quota_exhausted: true, ..Default::default() });
    assert!(poll_once(&w.cfg, &generate).await.unwrap());
    let (status, detail) = status_of(&w, id).await;
    assert_eq!((status.as_str(), detail.contains("额度用完")), ("failed", true), "{detail}");
    let n: i64 = sqlx::query_scalar("select count(*) from test_cases where origin = 'generated'").fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn 模型输出不合格由服务端整批拒收() {
    let w = world().await;
    RoleStore::open(&w.dir, real_probe()).set(vec![writer_role("w1", ModelSource::ClaudeCli)]).unwrap();
    let id = enqueue(&w, "语聊房", None).await;
    // 第二条写了明文密码：整批都不能入库，原因里不回显密码
    let bad = format!("{GOOD}---\nid: G3\ntitle: 登录\nsteps:\n  - 输入密码：hunter2\nexpected:\n  - 登录成功\n");
    let generate = fake(&Seen::default(), move || Outcome { text: bad.clone(), ..Default::default() });
    assert!(poll_once(&w.cfg, &generate).await.unwrap());
    let (status, detail) = status_of(&w, id).await;
    assert_eq!(status, "failed");
    assert!(detail.contains("整批没有入库") && !detail.contains("hunter2"), "{detail}");
    let n: i64 = sqlx::query_scalar("select count(*) from test_cases where origin = 'generated'").fetch_one(w.st.pool()).await.unwrap();
    assert_eq!(n, 0, "不合格的整批不能入库，包括其中合格的那几条");
}
