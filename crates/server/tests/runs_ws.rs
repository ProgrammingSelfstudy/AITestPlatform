//! 节点连接与 run 状态机的联动：真实起服务、用 WebSocket 客户端扮演节点，验证掉线、重连、续跑决定。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use protocol::{
    Caps, CaseExpected, DeviceInfo, DeviceStatus, Envelope, Heartbeat, Hello, ModelSource, ModelSourceCap, NodeMsg, Parsed, ResumeDecision, RoleModel,
    ServerMsg,
};
use serde_json::json;
use server::cases::{import, NewCase};
use server::runs::{self, NewTask};
use server::test_support::test_state;
use server::{app, AppState, Config};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const TOKEN: &str = "t";
type Client = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn start() -> (SocketAddr, AppState) {
    let st = test_state(Config { node_token: TOKEN.into(), hello_timeout: Duration::from_secs(1), heartbeat_s: 1, lost_grace: Duration::from_secs(120) }).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let s2 = st.clone();
    tokio::spawn(async move { axum::serve(l, app(s2)).await.unwrap() });
    (addr, st)
}

fn dev(serial: &str, status: DeviceStatus) -> DeviceInfo {
    DeviceInfo { serial: serial.into(), platform: "android".into(), model: None, os_version: None, screen_w: Some(720), screen_h: Some(1600), status }
}

/// 服务端下发的节点密钥（按 node_id 记住），重连时自动带上，和真实节点的行为一致。
static SECRETS: std::sync::Mutex<Option<std::collections::HashMap<Uuid, String>>> = std::sync::Mutex::new(None);

fn secret_for(node_id: Option<Uuid>) -> Option<String> {
    SECRETS.lock().unwrap().as_ref()?.get(&node_id?).cloned()
}

fn hello(node_id: Option<Uuid>, devices: Vec<DeviceInfo>, running: Vec<Uuid>) -> String {
    let caps = Caps {
        node_version: "t".into(),
        os: "macos".into(),
        model_sources: vec![ModelSourceCap { source: ModelSource::ClaudeCli, vision: true }, ModelSourceCap { source: ModelSource::CodexExec, vision: true }],
    };
    serde_json::to_string(&Envelope::new(NodeMsg::Hello(Hello { token: TOKEN.into(), node_id, node_secret: secret_for(node_id), user_token: None, user_name: None, caps, devices, running_runs: running }))).unwrap()
}

async fn connect(a: SocketAddr, text: String) -> (Client, protocol::Welcome) {
    let (mut c, _) = tokio_tungstenite::connect_async(format!("ws://{a}/ws/node")).await.unwrap();
    c.send(Message::Text(text.into())).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), c.next()).await.expect("等 welcome 超时") {
            Some(Ok(Message::Text(t))) => {
                if let Parsed::Known(env) = protocol::parse::<ServerMsg>(t.as_str()).unwrap() {
                    if let ServerMsg::Welcome(w) = env.body {
                        if let Some(secret) = &w.node_secret {
                            SECRETS.lock().unwrap().get_or_insert_with(Default::default).insert(w.node_id, secret.clone());
                        }
                        return (c, w);
                    }
                    panic!("期望 welcome，实际 {:?}", env.body);
                }
            }
            other => panic!("连接异常：{other:?}"),
        }
    }
}

fn nc(code: &str) -> NewCase {
    NewCase {
        code: code.into(), title: "t".into(), level: String::new(), platforms: vec![], preconditions: vec![], steps: vec!["s".into()],
        expected: vec![CaseExpected { idx: 1, text: "e".into() }], data: json!({}), refs: vec![], note: String::new(), should_fail: false,
    }
}

async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    let end = tokio::time::Instant::now() + Duration::from_secs(5);
    while !f().await {
        assert!(tokio::time::Instant::now() < end, "等待超时：{what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn status(st: &AppState, run: Uuid) -> String {
    sqlx::query_scalar("select status from runs where id = $1").bind(run).fetch_one(st.pool()).await.unwrap()
}

async fn dev_status(st: &AppState, serial: &str) -> String {
    sqlx::query_scalar("select status from devices where serial = $1").bind(serial).fetch_one(st.pool()).await.unwrap()
}

/// 节点上线 → 建任务 → 领到 → 开始执行。返回 (连接, node_id, run_id)。
async fn running_run(a: SocketAddr, st: &AppState) -> (Client, Uuid, Uuid) {
    let (c, w) = connect(a, hello(None, vec![dev("D1", DeviceStatus::Idle)], vec![])).await;
    import(st.pool(), "p", &[nc("A")], &BTreeMap::new()).await.unwrap();
    let t = runs::create_task(st.pool(), &NewTask { project: "p".into(), node_id: w.node_id, case_codes: vec!["A".into()], executor: RoleModel { source: ModelSource::CodexExec, model: None, effort: None }, judge: RoleModel { source: ModelSource::ClaudeCli, model: None, effort: None } }).await.unwrap();
    let claimed = runs::claim_next(st.pool(), w.node_id).await.unwrap().expect("应能领到");
    assert_eq!(claimed.run_id, t.run_ids[0]);
    runs::mark_started(st.pool(), w.node_id, claimed.run_id).await.unwrap();
    (c, w.node_id, claimed.run_id)
}

#[tokio::test]
async fn 断线_执行中的run转node_lost_设备仍被占着() {
    let (a, st) = start().await;
    let (c, _node, run) = running_run(a, &st).await;
    assert_eq!(status(&st, run).await, "running");
    drop(c);
    wait_for("run 转 node_lost", || async { status(&st, run).await == "node_lost" }).await;
    assert_eq!(dev_status(&st, "D1").await, "offline");
}

#[tokio::test]
async fn 重连_宽限期内声称仍在跑_服务端决定继续() {
    let (a, st) = start().await;
    let (c, node, run) = running_run(a, &st).await;
    drop(c);
    wait_for("node_lost", || async { status(&st, run).await == "node_lost" }).await;

    let (_c2, w) = connect(a, hello(Some(node), vec![dev("D1", DeviceStatus::Idle)], vec![run])).await;
    assert_eq!(w.resume.len(), 1);
    assert_eq!((w.resume[0].run_id, w.resume[0].decision), (run, ResumeDecision::Continue), "宽限期内重连要续跑，不是一律放弃");
    assert_eq!(status(&st, run).await, "running");
    // 节点重连时报的设备是「空闲」，但这台设备上有活跃的 run，服务端说了算：必须是 busy
    assert_eq!(dev_status(&st, "D1").await, "busy", "节点上报的空闲不能把设备锁覆盖掉");
}

#[tokio::test]
async fn 重连_超过宽限期_服务端让节点放弃() {
    let (a, st) = start().await;
    let (c, node, run) = running_run(a, &st).await;
    drop(c);
    wait_for("node_lost", || async { status(&st, run).await == "node_lost" }).await;
    sqlx::query("update runs set node_lost_at = now() - interval '10 minutes' where id = $1").bind(run).execute(st.pool()).await.unwrap();

    let (_c2, w) = connect(a, hello(Some(node), vec![dev("D1", DeviceStatus::Idle)], vec![run])).await;
    assert_eq!(w.resume[0].decision, ResumeDecision::Abort, "掉线太久了，不能续跑");
    assert_ne!(status(&st, run).await, "running");
}

#[tokio::test]
async fn 重连_节点没有这个run_判受阻并释放设备() {
    let (a, st) = start().await;
    let (c, node, run) = running_run(a, &st).await;
    drop(c);
    wait_for("node_lost", || async { status(&st, run).await == "node_lost" }).await;
    // 节点重启过：本地没有任何 run
    let (_c2, w) = connect(a, hello(Some(node), vec![dev("D1", DeviceStatus::Idle)], vec![])).await;
    assert!(w.resume.is_empty());
    assert_eq!(status(&st, run).await, "blocked");
    assert_eq!(dev_status(&st, "D1").await, "idle", "受阻后设备要释放");
}

#[tokio::test]
async fn 心跳里的空闲不能覆盖正在执行的设备() {
    let (a, st) = start().await;
    let (mut c, _node, run) = running_run(a, &st).await;
    assert_eq!(dev_status(&st, "D1").await, "busy");
    c.send(Message::Text(serde_json::to_string(&Envelope::new(NodeMsg::Heartbeat(Heartbeat { devices: vec![dev("D1", DeviceStatus::Idle)] }))).unwrap().into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(dev_status(&st, "D1").await, "busy", "设备锁由服务端说了算，节点说空闲也不行");
    assert_eq!(status(&st, run).await, "running");
}

#[tokio::test]
async fn 断线_刚分配还没开始的run退回排队() {
    let (a, st) = start().await;
    let (c, w) = connect(a, hello(None, vec![dev("D1", DeviceStatus::Idle)], vec![])).await;
    import(st.pool(), "p", &[nc("A")], &BTreeMap::new()).await.unwrap();
    let t = runs::create_task(st.pool(), &NewTask { project: "p".into(), node_id: w.node_id, case_codes: vec!["A".into()], executor: RoleModel { source: ModelSource::CodexExec, model: None, effort: None }, judge: RoleModel { source: ModelSource::ClaudeCli, model: None, effort: None } }).await.unwrap();
    runs::claim_next(st.pool(), w.node_id).await.unwrap().unwrap();
    assert_eq!(status(&st, t.run_ids[0]).await, "assigned");
    drop(c);
    wait_for("退回排队", || async { status(&st, t.run_ids[0]).await == "queued" }).await;
}

async fn next_message(client: &mut Client) -> ServerMsg {
    tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(Ok(frame)) = client.next().await {
            if let Message::Text(text) = frame {
                if let Parsed::Known(parsed) = protocol::parse::<ServerMsg>(&text).unwrap() {
                    return parsed.body;
                }
            }
        }
        panic!("没有收到协议消息");
    }).await.unwrap()
}

#[tokio::test]
async fn 启动确认丢失后只接受同一回执重传且旧连接收尾不影响新连接() {
    let (addr, state) = start().await;
    let devices = || vec![dev("D1", DeviceStatus::Idle)];
    let (mut old, registered) = connect(addr, hello(None, devices(), vec![])).await;
    let node = registered.node_id;
    assert!(registered.ack_run_updates);
    import(state.pool(), "p", &[nc("A")], &BTreeMap::new()).await.unwrap();
    let task = runs::create_task(state.pool(), &NewTask {
        project: "p".into(), node_id: node, case_codes: vec!["A".into()],
        executor: RoleModel { source: ModelSource::CodexExec, model: None, effort: None },
        judge: RoleModel { source: ModelSource::ClaudeCli, model: None, effort: None },
    }).await.unwrap();
    let run_id = task.run_ids[0];
    assert!(matches!(next_message(&mut old).await, ServerMsg::RunAssign(assign) if assign.run_id == run_id));
    let begun = Envelope::new(NodeMsg::RunStarted { run_id });
    let frame = || Message::Text(serde_json::to_string(&begun).unwrap().into());
    old.send(frame()).await.unwrap();
    wait_for("开始执行已落库", || async { status(&state, run_id).await == "running" }).await;

    let (mut new, resumed) = connect(addr, hello(Some(node), devices(), vec![run_id])).await;
    assert_eq!(resumed.resume[0].decision, ResumeDecision::Continue);
    drop(old);
    new.send(frame()).await.unwrap();
    assert!(matches!(next_message(&mut new).await, ServerMsg::Ack { ack_id } if ack_id == begun.id));
    let repeated = Envelope::new(NodeMsg::RunStarted { run_id });
    new.send(Message::Text(serde_json::to_string(&repeated).unwrap().into())).await.unwrap();
    assert!(matches!(next_message(&mut new).await, ServerMsg::Error(error) if error.ack_id == Some(repeated.id)));
    assert_eq!(status(&state, run_id).await, "running");
    assert_eq!(dev_status(&state, "D1").await, "busy");
}
