//! 节点 ↔ 服务端端到端：真实起服务端和节点（设备用假的，不依赖真机）。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use node::{run, Devices, NoExecutor, NodeConfig, NodeError, Reporter, RunExecutor};
use protocol::{BlockedReason, Caps, DeviceInfo, DeviceStatus, FinalResult, JudgeStatus, ModelSource, ModelSourceCap, RoleModel, RunAssign, Verdict};
use serde_json::json;
use server::accounts::{AccountKey, NewAccount};
use server::runs::{NewTask, RunView};
use server::test_support::{new_schema, state_for_schema, state_for_schema_with_accounts};
use server::{app, Config};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

const TOKEN: &str = "e2e-token";

struct Fake(Vec<DeviceInfo>);

impl Devices for Fake {
    async fn list(&self) -> Vec<DeviceInfo> {
        self.0.clone()
    }
}

fn dev(serial: &str) -> DeviceInfo {
    DeviceInfo {
        serial: serial.into(),
        platform: "android".into(),
        model: Some("X6531".into()),
        os_version: Some("14".into()),
        screen_w: Some(720),
        screen_h: Some(1600),
        status: DeviceStatus::Idle,
    }
}

fn server_cfg() -> Config {
    Config { node_token: TOKEN.into(), hello_timeout: Duration::from_secs(1), heartbeat_s: 1, lost_grace: Duration::from_secs(120) }
}

/// 一个独立运行时里的服务端。`kill()` 把整个运行时关掉：监听 socket 和所有已建立的连接一并断开，
/// 才能模拟「服务端进程宕机」。只 abort 监听任务不行——已建立的连接任务还会继续跑。
struct TestServer {
    rt: Option<tokio::runtime::Runtime>,
}

impl Drop for TestServer {
    /// 在异步上下文里不能直接 drop 运行时（会 panic），统一用 shutdown_background 收尾。
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

impl TestServer {
    /// `url`/`schema` 指定数据库和 schema；重启场景下两次启动共用同一个 schema，数据才会留下来。
    fn start_at(addr: &str, url: &str, schema: &str) -> (SocketAddr, TestServer) {
        Self::start_with(addr, url, schema, server_cfg(), None)
    }

    fn start_with(addr: &str, url: &str, schema: &str, cfg: Config, account_key: Option<AccountKey>) -> (SocketAddr, TestServer) {
        let (url, schema) = (url.to_string(), schema.to_string());
        let std_l = std::net::TcpListener::bind(addr).unwrap();
        std_l.set_nonblocking(true).unwrap();
        let bound = std_l.local_addr().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.spawn(async move {
            // 连接池建在服务端自己的运行时里：宕机（关运行时）时连接一并断掉，和真实进程退出一致
            let state = state_for_schema_with_accounts(&url, &schema, cfg, account_key, "debug").await;
            state.spawn_housekeeping(Duration::from_millis(100));
            let listener = tokio::net::TcpListener::from_std(std_l).unwrap();
            let _ = axum::serve(listener, app(state)).await;
        });
        (bound, TestServer { rt: Some(rt) })
    }

    fn kill(self) {
        drop(self); // Drop 里会关掉运行时
    }
}

fn node_cfg(addr: SocketAddr, token: &str) -> (NodeConfig, PathBuf) {
    let dir = std::env::temp_dir().join(format!("node_e2e_{}", Uuid::new_v4()));
    let mut cfg = NodeConfig::new(format!("ws://{addr}/ws/node"), token.into(), dir.clone());
    cfg.backoff_base = Duration::from_millis(50);
    cfg.backoff_cap = Duration::from_millis(200);
    (cfg, dir)
}

async fn nodes(addr: SocketAddr) -> Vec<serde_json::Value> {
    match reqwest::get(format!("http://{addr}/api/nodes")).await {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(_) => vec![],
    }
}

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

/// 等到服务端确实连不上了（HTTP 请求失败）。
async fn wait_nodes_unreachable(addr: SocketAddr) {
    let end = tokio::time::Instant::now() + Duration::from_secs(5);
    while reqwest::get(format!("http://{addr}/health")).await.is_ok() {
        assert!(tokio::time::Instant::now() < end, "服务端没有真正停下来");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn 节点上线并上报设备() {
    let (db, schema) = new_schema().await;
    let (addr, _srv) = TestServer::start_at("127.0.0.1:0", &db, &schema);
    let (cfg, dir) = node_cfg(addr, TOKEN);
    let task = tokio::spawn(run(cfg, Fake(vec![dev("1263040493010797"), dev("15064060")]), NoExecutor));

    let n = wait_nodes(addr, 5, |n| n.len() == 1 && n[0]["online"] == true).await;
    assert_eq!(n[0]["devices"].as_array().unwrap().len(), 2);
    assert!(node::load_node_id(&dir).is_some(), "welcome 之后 node_id 应已落盘");
    task.abort();
}

#[tokio::test]
async fn 服务端重启后节点自动重连且node_id不变() {
    let (db, schema) = new_schema().await;
    let (addr, srv) = TestServer::start_at("127.0.0.1:0", &db, &schema);
    let (cfg, dir) = node_cfg(addr, TOKEN);
    let task = tokio::spawn(run(cfg, Fake(vec![dev("a")]), NoExecutor));
    let before = wait_nodes(addr, 5, |n| n.len() == 1 && n[0]["online"] == true).await;
    let id = before[0]["node_id"].clone();

    // 服务端宕机（连接全部断开），节点反复重连失败；然后在同一个端口恢复
    srv.kill();
    wait_nodes_unreachable(addr).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (_addr2, _srv2) = TestServer::start_at(&addr.to_string(), &db, &schema);

    let after = wait_nodes(addr, 8, |n| n.len() == 1 && n[0]["online"] == true).await;
    assert_eq!(after[0]["node_id"], id, "重连必须带上原来的 node_id，才算同一个节点");
    assert_eq!(node::load_node_id(&dir).map(|u| u.to_string()), id.as_str().map(String::from));
    task.abort();
}

#[tokio::test]
async fn 令牌错误时节点停止而不是无限重试() {
    let (db, schema) = new_schema().await;
    let (addr, _srv) = TestServer::start_at("127.0.0.1:0", &db, &schema);
    let (cfg, _dir) = node_cfg(addr, "错误的令牌");
    let r = tokio::time::timeout(Duration::from_secs(5), run(cfg, Fake(vec![dev("a")]), NoExecutor))
        .await
        .expect("被拒绝后 run 应立即返回，而不是一直重试");
    match r {
        Err(NodeError::Rejected { code, .. }) => assert_eq!(code, "unauthorized"),
        other => panic!("应当返回 Rejected，实际：{other:?}"),
    }
    assert!(nodes(addr).await.is_empty(), "被拒绝的节点不能出现在列表里");
}

#[tokio::test]
async fn 服务端不在时节点持续重试而不退出() {
    // 先占一个端口再释放，保证这个地址上确实没人监听
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let (cfg, _dir) = node_cfg(addr, TOKEN);
    let task = tokio::spawn(run(cfg, Fake(vec![]), NoExecutor));
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!task.is_finished(), "连不上服务端时节点应继续退避重试，不能退出");
    task.abort();
}

struct LinkSwitch {
    addr: SocketAddr,
    enabled: watch::Sender<bool>,
    listener: JoinHandle<()>,
}

impl LinkSwitch {
    async fn new(server: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (enabled, state) = watch::channel(true);
        let listener = tokio::spawn(async move {
            while let Ok((mut downstream, _)) = listener.accept().await {
                let mut state = state.clone();
                if !*state.borrow_and_update() {
                    continue;
                }
                tokio::spawn(async move {
                    let Ok(mut upstream) = tokio::net::TcpStream::connect(server).await else { return };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream) => {}
                        _ = state.wait_for(|enabled| !*enabled) => {}
                    }
                });
            }
        });
        Self { addr, enabled, listener }
    }
}

impl Drop for LinkSwitch {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

#[derive(Clone)]
struct Probe {
    real: bool,
    timeout: bool,
    entered: mpsc::UnboundedSender<Uuid>,
    returned: mpsc::UnboundedSender<Uuid>,
    finish: Arc<Semaphore>,
}

struct ReportReturn(Uuid, mpsc::UnboundedSender<Uuid>);

impl Drop for ReportReturn {
    fn drop(&mut self) {
        let _ = self.1.send(self.0);
    }
}

impl RunExecutor for Probe {
    async fn execute(&self, assign: RunAssign, rep: Reporter) {
        let _on_return = ReportReturn(assign.run_id, self.returned.clone());
        assert!(assign.lease_id.is_some(), "隔离任务应拿到测试租约");
        rep.started();
        let android = device::Android::new(assign.device_serial);
        if self.real {
            let image = android.screenshot_png().await.expect("真机首次只读截图失败");
            assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
        }
        rep.step(1, 0, "observe", json!({}), json!({"probe": true}), None);
        self.entered.send(assign.run_id).unwrap();
        if self.timeout {
            assert!(tokio::time::timeout(Duration::from_millis(600), self.finish.acquire()).await.is_err());
            return rep.blocked(BlockedReason::Stuck, "隔离探针超时".into());
        }
        self.finish.acquire().await.unwrap().forget();
        if self.real {
            let image = android.screenshot_png().await.expect("真机第二次只读截图失败");
            assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));
        }
        rep.step(1, 0, "observe", json!({}), json!({"probe": true}), None);
        rep.step(2, 0, "observe", json!({}), json!({"probe": true}), None);
        rep.executor_done(Verdict::Uncertain, "只读探针不判业务结果".into(), None);
        rep.judge_done(JudgeStatus::NotRun, FinalResult::Uncertain, vec![]);
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    ShortCut,
    ServerRestart,
    PastGrace,
    NodeRestart,
    CancelOffline,
    ExecutorTimeout,
}

async fn until_run(pool: &sqlx::PgPool, task: Uuid, status: &str) -> RunView {
    tokio::time::timeout(Duration::from_secs(18), async {
        loop {
            let run = server::runs::task_runs(pool, task).await.unwrap().pop().unwrap();
            if run.status == status {
                return run;
            }
            assert!(!matches!(run.status.as_str(), "blocked" | "finished" | "cancelled"), "期望 {status}，实际 {run:?}");
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }).await.unwrap_or_else(|_| panic!("任务状态未转为 {status}"))
}

async fn recv_probe(events: &mut mpsc::UnboundedReceiver<Uuid>, run_id: Uuid) {
    assert_eq!(tokio::time::timeout(Duration::from_secs(15), events.recv()).await.unwrap(), Some(run_id));
}

async fn wait_step_count(pool: &sqlx::PgPool, run_id: Uuid, count: i64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let got: i64 = sqlx::query_scalar("select count(*) from run_steps where run_id = $1").bind(run_id).fetch_one(pool).await.unwrap();
            if got == count {
                return;
            }
            assert!(got < count, "步骤重复落库：{got} > {count}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("步骤未落库");
}

async fn recovery_case(fault: Fault, serial: &str, real: bool) {
    eprintln!("恢复验收场景：{fault:?}");
    let (db, schema) = new_schema().await;
    let grace = if matches!(fault, Fault::PastGrace) { Duration::from_secs(2) } else { Duration::from_secs(60) };
    let cfg = Config { lost_grace: grace, ..server_cfg() };
    let control = state_for_schema(&db, &schema, cfg.clone()).await;
    let pool = control.pool();
    let test_key = || AccountKey::from_hex(&"ab".repeat(32)).unwrap();
    let (addr, server) = TestServer::start_with("127.0.0.1:0", &db, &schema, cfg.clone(), Some(test_key()));
    let mut server = Some(server);
    let link = LinkSwitch::new(addr).await;
    let (node, dir) = node_cfg(link.addr, TOKEN);
    let (enter, mut entered) = mpsc::unbounded_channel();
    let (exit, mut returned) = mpsc::unbounded_channel();
    let probe = Probe { real, timeout: matches!(fault, Fault::ExecutorTimeout), entered: enter, returned: exit, finish: Arc::new(Semaphore::new(0)) };
    let mut node_task = tokio::spawn(run(node.clone(), Fake(vec![dev(serial)]), probe.clone()));
    let listed = wait_nodes(addr, 12, |list| list.len() == 1 && list[0]["online"] == true).await;
    let owner: Uuid = serde_json::from_value(listed[0]["node_id"].clone()).unwrap();
    let mut caps: Caps = serde_json::from_value(listed[0]["caps"].clone()).unwrap();
    caps.model_sources = vec![
        ModelSourceCap { source: ModelSource::CodexExec, vision: true },
        ModelSourceCap { source: ModelSource::ClaudeCli, vision: true },
    ];
    sqlx::query("update nodes set caps = $2 where id = $1").bind(owner)
        .bind(serde_json::to_value(caps).unwrap()).execute(pool).await.unwrap();
    let case = server::cases::parse_case("id: RECOVERY\ntitle: 只读恢复探针\nsteps: [观察]\nexpected: [有读数]\ndata:\n  account: '{{account.username}}'", "fixture").unwrap();
    server::cases::import(pool, "isolated", &[case], &BTreeMap::new()).await.unwrap();
    server::accounts::import_accounts(pool, &test_key(), "isolated", "debug", &[NewAccount {
        username: "synthetic-account".into(), password: "not-a-live-password".into(), nickname: "测试租约".into(),
        tags: vec![], device_serial: Some(serial.into()), identity: None,
    }]).await.unwrap();
    let role = |source| RoleModel { source, model: None, effort: None };
    let task = server::runs::create_task(pool, &NewTask {
        project: "isolated".into(), node_id: owner, case_codes: vec!["RECOVERY".into()],
        executor: role(ModelSource::CodexExec), judge: role(ModelSource::ClaudeCli),
    }).await.unwrap();
    let run_id = task.run_ids[0];
    recv_probe(&mut entered, run_id).await;
    wait_step_count(pool, run_id, 1).await;
    if !matches!(fault, Fault::ExecutorTimeout) {
        until_run(pool, task.task_id, "running").await;
    }

    let (status, reason, dirty) = match fault {
        Fault::ShortCut | Fault::ServerRestart => {
            if matches!(fault, Fault::ServerRestart) {
                server.take().unwrap().kill();
                wait_nodes_unreachable(addr).await;
            } else {
                link.enabled.send(false).unwrap();
                until_run(pool, task.task_id, "node_lost").await;
            }
            let still_leased: bool = sqlx::query_scalar("select released_at is null from account_leases where run_id = $1").bind(run_id).fetch_one(pool).await.unwrap();
            assert!(still_leased, "宽限期内不能释放租约");
            probe.finish.add_permits(1);
            recv_probe(&mut returned, run_id).await;
            tokio::time::sleep(Duration::from_millis(80)).await;
            if matches!(fault, Fault::ServerRestart) {
                server = Some(TestServer::start_with(&addr.to_string(), &db, &schema, cfg, Some(test_key())).1);
            } else {
                link.enabled.send(true).unwrap();
            }
            ("finished", None, false)
        }
        Fault::PastGrace | Fault::CancelOffline => {
            link.enabled.send(false).unwrap();
            until_run(pool, task.task_id, "node_lost").await;
            let expected = if matches!(fault, Fault::CancelOffline) {
                control.cancel_run(run_id).await.unwrap();
                ("cancelled", None, true)
            } else {
                ("blocked", Some("node_lost"), true)
            };
            until_run(pool, task.task_id, expected.0).await;
            link.enabled.send(true).unwrap();
            recv_probe(&mut returned, run_id).await;
            expected
        }
        Fault::NodeRestart => {
            node_task.abort();
            let _ = node_task.await;
            recv_probe(&mut returned, run_id).await;
            until_run(pool, task.task_id, "node_lost").await;
            node_task = tokio::spawn(run(node, Fake(vec![dev(serial)]), probe.clone()));
            ("blocked", Some("node_lost"), true)
        }
        Fault::ExecutorTimeout => ("blocked", Some("stuck"), true),
    };
    let result = until_run(pool, task.task_id, status).await;
    assert_eq!(result.blocked_reason.as_deref(), reason);
    assert_ne!(result.final_result.as_deref(), Some("passed"));
    wait_step_count(pool, run_id, if status == "finished" { 2 } else { 1 }).await;
    let leases: Vec<(bool, bool)> = sqlx::query_as("select released_at is not null, dirty from account_leases where run_id = $1")
        .bind(run_id).fetch_all(pool).await.unwrap();
    assert_eq!(leases, vec![(true, dirty)]);
    wait_nodes(addr, 12, |list| list.len() == 1 && list[0]["online"] == true && list[0]["node_id"] == owner.to_string() && list[0]["devices"][0]["status"] == "idle").await;
    assert!(entered.try_recv().is_err(), "同一 run 不得重启第二份执行");
    if real {
        eprintln!("真机只读探针 {fault:?}: run={run_id}, status={status}, reason={reason:?}, lease_released=true");
    }
    node_task.abort();
    drop(server);
    std::fs::remove_dir_all(dir).unwrap();
}

const FAULTS: [Fault; 6] = [Fault::ShortCut, Fault::ServerRestart, Fault::PastGrace, Fault::NodeRestart, Fault::CancelOffline, Fault::ExecutorTimeout];

#[tokio::test]
async fn 隔离恢复验收覆盖结果补发重启超宽限取消和超时() {
    for fault in FAULTS {
        recovery_case(fault, "recovery-probe", false).await;
    }
}

#[tokio::test]
#[ignore]
async fn 真机只读探针验证恢复链路() {
    let serial = std::env::var("HAFA_RECOVERY_TEST_SERIAL").expect("请明确指定获准的 Android 测试设备");
    assert!(device::list_devices().await.unwrap().iter().any(|entry| entry.serial == serial), "指定测试设备未连接");
    for fault in FAULTS {
        recovery_case(fault, &serial, true).await;
    }
}
