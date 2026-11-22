//! 节点接入层的集成测试：真实起服务、用 WebSocket 客户端扮演节点。

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use protocol::{
    Caps, DeviceInfo, DeviceStatus, Envelope, Heartbeat, Hello, ModelSource, ModelSourceCap, NodeMsg, Parsed,
    ServerMsg,
};
use server::test_support::test_state;
use server::{app, Config};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const TOKEN: &str = "test-token";

type Client = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 起一个服务：心跳 1 秒（3 秒无消息判离线），hello 限时 1 秒。
async fn start() -> SocketAddr {
    let state = test_state(Config { node_token: TOKEN.into(), hello_timeout: Duration::from_secs(1), heartbeat_s: 1, lost_grace: Duration::from_secs(120) }).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app(state)).await.unwrap() });
    addr
}

fn device(serial: &str, status: DeviceStatus) -> DeviceInfo {
    DeviceInfo {
        serial: serial.into(),
        platform: "android".into(),
        model: Some("X6531".into()),
        os_version: Some("14".into()),
        screen_w: Some(720),
        screen_h: Some(1600),
        status,
    }
}

/// 服务端下发的节点密钥（按 node_id 记住），重连时自动带上，和真实节点的行为一致。
static SECRETS: std::sync::Mutex<Option<std::collections::HashMap<Uuid, String>>> = std::sync::Mutex::new(None);

fn secret_for(node_id: Option<Uuid>) -> Option<String> {
    SECRETS.lock().unwrap().as_ref()?.get(&node_id?).cloned()
}

fn hello(token: &str, node_id: Option<Uuid>, devices: Vec<DeviceInfo>) -> String {
    let body = NodeMsg::Hello(Hello {
        token: token.into(),
        node_id,
        node_secret: secret_for(node_id),
        user_token: None,
        caps: Caps {
            node_version: "test".into(),
            os: "macos".into(),
            model_sources: vec![ModelSourceCap { source: ModelSource::ClaudeCli, vision: true }],
        },
        devices,
        running_runs: vec![],
    });
    serde_json::to_string(&Envelope::new(body)).unwrap()
}

async fn connect(addr: SocketAddr) -> Client {
    tokio_tungstenite::connect_async(format!("ws://{addr}/ws/node")).await.unwrap().0
}

async fn send(c: &mut Client, text: String) {
    c.send(Message::Text(text.into())).await.unwrap();
}

/// 读下一条服务端消息；连接被关闭返回 None。
async fn next_msg(c: &mut Client) -> Option<ServerMsg> {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), c.next()).await.expect("等服务端消息超时") {
            Some(Ok(Message::Text(t))) => match protocol::parse::<ServerMsg>(t.as_str()).unwrap() {
                Parsed::Known(env) => {
                    if let ServerMsg::Welcome(w) = &env.body {
                        if let Some(secret) = &w.node_secret {
                            SECRETS.lock().unwrap().get_or_insert_with(Default::default).insert(w.node_id, secret.clone());
                        }
                    }
                    return Some(env.body);
                }
                Parsed::UnknownType { .. } => continue,
            },
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => continue,
        }
    }
}

async fn nodes(addr: SocketAddr) -> Vec<serde_json::Value> {
    reqwest::get(format!("http://{addr}/api/nodes")).await.unwrap().json().await.unwrap()
}

/// 轮询 /api/nodes 直到条件成立（连接收尾是异步的）。
async fn wait_nodes(addr: SocketAddr, secs: u64, f: impl Fn(&[serde_json::Value]) -> bool) -> Vec<serde_json::Value> {
    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let n = nodes(addr).await;
        if f(&n) {
            return n;
        }
        assert!(tokio::time::Instant::now() < end, "等待条件超时，当前：{n:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn 健康检查() {
    let addr = start().await;
    let body = reqwest::get(format!("http://{addr}/health")).await.unwrap().text().await.unwrap();
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn 节点接入后能看到在线节点和设备() {
    let addr = start().await;
    let mut c = connect(addr).await;
    send(&mut c, hello(TOKEN, None, vec![device("1263040493010797", DeviceStatus::Idle)])).await;

    let Some(ServerMsg::Welcome(w)) = next_msg(&mut c).await else { panic!("应当先收到 welcome") };
    assert_eq!(w.heartbeat_s, 1);

    let n = wait_nodes(addr, 3, |n| n.len() == 1).await;
    assert_eq!(n[0]["online"], true);
    assert_eq!(n[0]["node_id"], w.node_id.to_string());
    assert_eq!(n[0]["devices"][0]["serial"], "1263040493010797");
}

#[tokio::test]
async fn 令牌错误被拒绝且不留注册记录() {
    let addr = start().await;
    let mut c = connect(addr).await;
    send(&mut c, hello("错误的令牌", None, vec![device("x", DeviceStatus::Idle)])).await;

    match next_msg(&mut c).await {
        Some(ServerMsg::Error(e)) => assert_eq!(e.code, "unauthorized"),
        other => panic!("应当收到 unauthorized，实际：{other:?}"),
    }
    assert!(next_msg(&mut c).await.is_none(), "拒绝后连接应被关闭");
    assert!(nodes(addr).await.is_empty(), "令牌错误不能留下节点记录");
}

#[tokio::test]
async fn 首条消息不是hello被拒绝() {
    let addr = start().await;
    let mut c = connect(addr).await;
    send(&mut c, serde_json::to_string(&Envelope::new(NodeMsg::Heartbeat(Heartbeat { devices: vec![] }))).unwrap()).await;
    match next_msg(&mut c).await {
        Some(ServerMsg::Error(e)) => assert_eq!(e.code, "protocol"),
        other => panic!("应当收到 protocol 错误，实际：{other:?}"),
    }
    assert!(nodes(addr).await.is_empty());
}

#[tokio::test]
async fn 连接后不发hello会超时断开() {
    let addr = start().await;
    let mut c = connect(addr).await;
    match next_msg(&mut c).await {
        Some(ServerMsg::Error(e)) => assert_eq!(e.code, "hello_timeout"),
        other => panic!("应当收到 hello_timeout，实际：{other:?}"),
    }
}

#[tokio::test]
async fn 断线后节点和设备都变离线() {
    let addr = start().await;
    let mut c = connect(addr).await;
    send(&mut c, hello(TOKEN, None, vec![device("a", DeviceStatus::Idle)])).await;
    next_msg(&mut c).await;
    wait_nodes(addr, 3, |n| n.len() == 1 && n[0]["online"] == true).await;

    drop(c);
    let n = wait_nodes(addr, 3, |n| n[0]["online"] == false).await;
    assert_eq!(n[0]["devices"][0]["status"], "offline", "节点离线时其下设备必须一起置离线");
}

#[tokio::test]
async fn 心跳会更新设备状态() {
    let addr = start().await;
    let mut c = connect(addr).await;
    send(&mut c, hello(TOKEN, None, vec![device("a", DeviceStatus::Idle)])).await;
    next_msg(&mut c).await;
    send(&mut c, serde_json::to_string(&Envelope::new(NodeMsg::Heartbeat(Heartbeat { devices: vec![device("a", DeviceStatus::Busy)] }))).unwrap()).await;
    wait_nodes(addr, 3, |n| n[0]["devices"][0]["status"] == "busy").await;
}

#[tokio::test]
async fn 重连带node_id时认作同一个节点() {
    let addr = start().await;
    let mut c1 = connect(addr).await;
    send(&mut c1, hello(TOKEN, None, vec![device("a", DeviceStatus::Idle)])).await;
    let Some(ServerMsg::Welcome(w1)) = next_msg(&mut c1).await else { panic!() };
    drop(c1);
    wait_nodes(addr, 3, |n| n[0]["online"] == false).await;

    let mut c2 = connect(addr).await;
    send(&mut c2, hello(TOKEN, Some(w1.node_id), vec![device("a", DeviceStatus::Idle)])).await;
    let Some(ServerMsg::Welcome(w2)) = next_msg(&mut c2).await else { panic!() };
    assert_eq!(w1.node_id, w2.node_id);
    let n = wait_nodes(addr, 3, |n| n[0]["online"] == true).await;
    assert_eq!(n.len(), 1, "重连不能产生第二条节点记录");
}

#[tokio::test]
async fn 首次连接下发密钥_重连缺少或带错密钥被拒绝() {
    let addr = start().await;
    let mut c1 = connect(addr).await;
    send(&mut c1, hello(TOKEN, None, vec![device("a", DeviceStatus::Idle)])).await;
    let Some(ServerMsg::Welcome(w)) = next_msg(&mut c1).await else { panic!() };
    assert!(w.node_secret.is_some(), "首次连接要下发节点密钥");
    drop(c1);
    wait_nodes(addr, 3, |n| n[0]["online"] == false).await;

    // 共享令牌对、node_id 对，但没有密钥：冒领被拒绝
    let forged = |secret: Option<&str>| {
        let mut env: serde_json::Value = serde_json::from_str(&hello(TOKEN, Some(w.node_id), vec![])).unwrap();
        match secret {
            Some(s) => env["payload"]["node_secret"] = serde_json::json!(s),
            None => {
                env["payload"].as_object_mut().unwrap().remove("node_secret");
            }
        }
        env.to_string()
    };
    for secret in [None, Some("错误的密钥")] {
        let mut c = connect(addr).await;
        send(&mut c, forged(secret)).await;
        match next_msg(&mut c).await {
            Some(ServerMsg::Error(e)) => assert_eq!(e.code, "unauthorized"),
            other => panic!("期望 unauthorized，实际 {other:?}"),
        }
    }
    let n = nodes(addr).await;
    assert_eq!(n[0]["online"], false, "被拒绝的冒领不能把节点标成在线");
}

#[tokio::test]
async fn 旧连接收尾不会把重连后的节点标成离线() {
    let addr = start().await;
    let mut old = connect(addr).await;
    send(&mut old, hello(TOKEN, None, vec![device("a", DeviceStatus::Idle)])).await;
    let Some(ServerMsg::Welcome(w)) = next_msg(&mut old).await else { panic!() };

    // 旧连接还没断，新连接已经用同一个 node_id 连上（网络闪断后的典型情况）
    let mut new = connect(addr).await;
    send(&mut new, hello(TOKEN, Some(w.node_id), vec![device("a", DeviceStatus::Idle)])).await;
    next_msg(&mut new).await;

    drop(old);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let n = nodes(addr).await;
    assert_eq!(n[0]["online"], true, "旧连接断开不能把新连接标成离线");
}

#[tokio::test]
async fn 未知消息类型不会断开连接() {
    let addr = start().await;
    let mut c = connect(addr).await;
    send(&mut c, hello(TOKEN, None, vec![])).await;
    next_msg(&mut c).await;
    send(&mut c, r#"{"v":1,"id":"00000000-0000-0000-0000-000000000000","ts":"2026-11-20T10:00:00Z","type":"from_the_future","payload":{}}"#.into()).await;
    send(&mut c, "完全不是 JSON".into()).await;
    // 之后连接仍然可用：心跳照常被接受
    send(&mut c, serde_json::to_string(&Envelope::new(NodeMsg::Heartbeat(Heartbeat { devices: vec![device("z", DeviceStatus::Idle)] }))).unwrap()).await;
    wait_nodes(addr, 3, |n| n[0]["devices"].as_array().is_some_and(|d| d.len() == 1) && n[0]["online"] == true).await;
}

#[tokio::test]
async fn 心跳超时判离线() {
    let addr = start().await;
    let mut c = connect(addr).await;
    send(&mut c, hello(TOKEN, None, vec![device("a", DeviceStatus::Idle)])).await;
    next_msg(&mut c).await;
    // 不再发任何消息：3 个心跳间隔（3 秒）后服务端判离线，保持 c 不 drop 以排除「断线」路径
    let n = wait_nodes(addr, 6, |n| n[0]["online"] == false).await;
    assert_eq!(n[0]["devices"][0]["status"], "offline");
    drop(c);
}
