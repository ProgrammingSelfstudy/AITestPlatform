//! 执行节点（P0：接入、心跳、断线重连、上报设备）。
//!
//! 节点**主动连**服务端，不需要公网 IP。P1 起再接任务执行。
//! 节点令牌从环境变量读取。测试账号凭证只走单独的本机 HTTP 接口，不经过 WebSocket。

pub mod casegen_run;
pub mod real;
pub mod params;
pub mod roles;
mod recorder;
pub mod runner;
pub mod scriptgen_run;
pub mod heal_run;
pub mod regress_poll;
pub mod regress_run;
pub mod selfcheck_run;
pub mod setup_runner;
mod uploader;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use device::{Android, Info};
use futures_util::{SinkExt, StreamExt};
use protocol::{
    BlockedReason, Caps, DeviceInfo, DeviceStatus, Envelope, Heartbeat, Hello, ModelSource, ModelSourceCap, NodeMsg, Parsed,
    ResumeDecision, RunAssign, RunBlocked, ServerMsg,
};
use tokio::sync::mpsc;
use thiserror::Error;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
pub use runner::{NoExecutor, Reporter, RunExecutor, Runner};
use uuid::Uuid;

/// 占住某台设备的本地锁，直到返回值被丢弃。节点自己的任务执行器、页面发起的回归都用同一把按序列号的锁；
/// 对外公开是为了让集成测试能模拟「设备正被本地的录屏和收尾占着」。
pub async fn hold_device(serial: &str) -> impl Send + Sync + 'static {
    recorder::lock_device(serial).await
}

pub(crate) fn platform_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(timeout)
        .build()
}

#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// 服务端地址，如 `ws://127.0.0.1:8080/ws/node`
    pub url: String,
    /// 节点令牌（来自环境变量，不写日志）
    pub token: String,
    /// 本地状态目录，存 `node_id`
    pub state_dir: PathBuf,
    /// 节点主人的个人令牌（来自 `NODE_USER_TOKEN`，不写日志）。首次连接时服务端据此绑定节点属主
    pub user_token: Option<String>,
    /// 节点主人的名字（来自 `NODE_USER_NAME`）。**只记名字，不是机密**：首次连接时服务端据此登记并绑定属主
    pub user_name: Option<String>,
    pub node_version: String,
    /// 重连退避：从 `backoff_base` 起每次翻倍，封顶 `backoff_cap`
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
}

impl NodeConfig {
    pub fn new(url: String, token: String, state_dir: PathBuf) -> Self {
        Self {
            url,
            token,
            state_dir,
            user_token: None,
            user_name: None,
            node_version: env!("CARGO_PKG_VERSION").to_string(),
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Error)]
pub enum NodeError {
    /// 令牌无效或协议版本过低：重试没有意义，必须人来处理
    #[error("服务端拒绝了本节点（{code}）：{message}")]
    Rejected { code: String, message: String },
}

// ---------------------------------------------------------------- 设备来源

/// 本机有哪些设备。抽成 trait 是为了测试时用假设备，不依赖真机。
pub trait Devices: Send + Sync {
    fn list(&self) -> impl Future<Output = Vec<DeviceInfo>> + Send;
}

/// 只暴露白名单里的设备。账号池会给空闲设备分账号，而「同一账号不能同时登在两台手机」，
/// 所以测试时可以只放一台，避免别的手机被卷进来。白名单为空表示不限制。
pub struct Only<D> {
    pub inner: D,
    pub serials: Vec<String>,
}

impl<D: Devices> Devices for Only<D> {
    async fn list(&self) -> Vec<DeviceInfo> {
        let all = self.inner.list().await;
        if self.serials.is_empty() {
            return all;
        }
        all.into_iter().filter(|d| self.serials.contains(&d.serial)).collect()
    }
}

/// 经 adb 发现的 Android 设备。设备信息（型号、系统、分辨率）按序列号缓存，避免每次心跳都跑一遍 adb。
#[derive(Default)]
pub struct AdbDevices {
    cache: Mutex<HashMap<String, Info>>,
}

impl Devices for AdbDevices {
    async fn list(&self) -> Vec<DeviceInfo> {
        let entries = match device::list_devices().await {
            Ok(e) => e,
            Err(e) => {
                // 设备命令失败：原始错误写日志，本轮当作没有设备，下一次心跳再试
                tracing::warn!("列设备失败：{e}");
                return vec![];
            }
        };
        let mut out = Vec::new();
        for e in entries {
            let online = e.state == "device";
            let cached = self.cache.lock().unwrap().get(&e.serial).cloned();
            let info = match (online, cached) {
                (true, Some(i)) => Some(i),
                (true, None) => match Android::new(e.serial.clone()).info().await {
                    Ok(i) => {
                        self.cache.lock().unwrap().insert(e.serial.clone(), i.clone());
                        Some(i)
                    }
                    Err(err) => {
                        tracing::warn!(serial = %e.serial, "读设备信息失败：{err}");
                        None
                    }
                },
                _ => None,
            };
            out.push(DeviceInfo {
                serial: e.serial,
                platform: "android".into(),
                model: info.as_ref().and_then(|i| i.model.clone()).or(e.model),
                os_version: info.as_ref().and_then(|i| i.os_version.clone()),
                screen_w: info.as_ref().and_then(|i| i.screen_w),
                screen_h: info.as_ref().and_then(|i| i.screen_h),
                // unauthorized / offline 等都算离线；P1 起再区分忙闲
                status: if online { DeviceStatus::Idle } else { DeviceStatus::Offline },
            });
        }
        out
    }
}

// ---------------------------------------------------------------- 本机能力

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// 本机可用的模型来源：只看对应命令行工具在不在。是否真的已登录会员、额度够不够，P0 后半段单独验证。
/// `vision` 来自技术验证里实测过两条路径都能带图执行，不是探测结果。
pub fn detect_caps(node_version: &str) -> Caps {
    let mut model_sources = vec![];
    if on_path("claude") {
        model_sources.push(ModelSourceCap { source: ModelSource::ClaudeCli, vision: true });
    }
    if on_path("codex") {
        model_sources.push(ModelSourceCap { source: ModelSource::CodexExec, vision: true });
    }
    if on_path("agy") {
        model_sources.push(ModelSourceCap { source: ModelSource::Agy, vision: true });
    }
    Caps { node_version: node_version.into(), os: std::env::consts::OS.into(), model_sources }
}

// ---------------------------------------------------------------- node_id 持久化

fn node_id_path(dir: &Path) -> PathBuf {
    dir.join("node_id")
}

pub fn load_node_id(dir: &Path) -> Option<Uuid> {
    std::fs::read_to_string(node_id_path(dir)).ok().and_then(|s| s.trim().parse().ok())
}

// ---------------------------------------------------------------- 节点身份密钥

static NODE_SECRET: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

fn node_secret_path(dir: &Path) -> PathBuf {
    dir.join("node_secret")
}

/// 读本地保存的密钥并放进进程内存。没有返回 `None`。
pub fn load_node_secret(dir: &Path) -> Option<String> {
    let secret = std::fs::read_to_string(node_secret_path(dir)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    *NODE_SECRET.write().unwrap() = secret.clone();
    secret
}

/// 保存密钥（0600）。保存失败不继续：密钥丢了下次重连会被服务端拒绝，必须让人知道。
fn save_node_secret(dir: &Path, secret: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = node_secret_path(dir);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?.write_all(secret.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, secret)?;
    *NODE_SECRET.write().unwrap() = Some(secret.to_string());
    Ok(())
}

/// 给节点发往平台的 HTTP 请求加上身份密钥。
pub(crate) fn with_node_secret(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match NODE_SECRET.read().unwrap().as_deref() {
        Some(secret) => request.header("X-Node-Secret", secret),
        None => request,
    }
}

fn save_node_id(dir: &Path, id: Uuid) {
    if let Err(e) = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(node_id_path(dir), id.to_string())) {
        tracing::warn!("保存 node_id 失败（下次重连会被当成新节点）：{e}");
    }
}

// ---------------------------------------------------------------- 重连退避

/// 第 `attempt` 次重试前的等待：`base × 2^attempt`，封顶 `cap`，再乘 0.8～1.2 的抖动。
/// `jitter` 取 [0, 1)，单独传入是为了可测试。
pub fn backoff(base: Duration, cap: Duration, attempt: u32, jitter: f64) -> Duration {
    let raw = base.saturating_mul(1u32.checked_shl(attempt.min(20)).unwrap_or(u32::MAX)).min(cap);
    raw.mul_f64(0.8 + 0.4 * jitter.clamp(0.0, 1.0))
}

// ---------------------------------------------------------------- 一次会话

enum SessionEnd {
    /// 连接丢了或没连上，`connected` 表示这次是否曾经成功握手（成功过就重置退避）
    Lost { connected: bool, reason: String },
    Rejected(NodeError),
}

fn envelope_text(message: &Envelope<NodeMsg>) -> Message {
    Message::Text(serde_json::to_string(message).expect("协议消息序列化不会失败").into())
}

struct PendingUpdate {
    message: Envelope<NodeMsg>,
    start: Option<RunAssign>,
}

struct Outbox {
    rx: mpsc::UnboundedReceiver<NodeMsg>,
    pending: VecDeque<PendingUpdate>,
}

impl Outbox {
    fn resume_runs(&mut self, running: Vec<Uuid>) -> Vec<Uuid> {
        while let Ok(body) = self.rx.try_recv() {
            if body.tracked_run().is_some() {
                self.pending.push_back(PendingUpdate { message: Envelope::new(body), start: None });
            }
        }
        let mut runs = running;
        runs.extend(self.pending.iter().filter_map(|update| update.message.body.tracked_run()));
        runs.sort_unstable();
        runs.dedup();
        runs
    }

    fn discard_run(&mut self, run_id: Uuid) {
        self.pending.retain(|update| update.message.body.tracked_run() != Some(run_id));
    }

    fn receipt(&mut self, ack_id: Uuid) -> Option<PendingUpdate> {
        let index = self.pending.iter().position(|update| update.message.id == ack_id)?;
        self.pending.remove(index)
    }
}

async fn session<D: Devices, E: RunExecutor>(cfg: &NodeConfig, devices: &D, runner: &Runner<E>, outbox: &mut Outbox, node_id: &mut Option<Uuid>, roles: &Arc<roles::RoleStore>, role_tx: &mpsc::UnboundedSender<NodeMsg>) -> SessionEnd {
    let lost = |connected: bool, reason: String| SessionEnd::Lost { connected, reason };

    let mut ws = match connect_async(cfg.url.as_str()).await {
        Ok((ws, _)) => ws,
        Err(e) => return lost(false, format!("连接失败：{e}")),
    };

    let available_devices = devices.list().await;
    let running_runs = outbox.resume_runs(runner.running());
    let hello = NodeMsg::Hello(Hello {
        token: cfg.token.clone(),
        node_id: *node_id,
        node_secret: load_node_secret(&cfg.state_dir),
        user_token: cfg.user_token.clone(),
        user_name: cfg.user_name.clone(),
        caps: detect_caps(&cfg.node_version),
        devices: available_devices,
        running_runs,
    });
    if let Err(e) = ws.send(envelope_text(&Envelope::new(hello))).await {
        return lost(false, format!("发送 hello 失败：{e}"));
    }

    // 等 welcome（或拒绝）
    let (authenticated_node, heartbeat_s, resume, ack_run_updates) = loop {
        let msg = match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Err(_) => return lost(false, "等待 welcome 超时".into()),
            Ok(None) | Ok(Some(Err(_))) => return lost(false, "握手阶段连接被关闭".into()),
            Ok(Some(Ok(m))) => m,
        };
        let Message::Text(t) = msg else { continue };
        match protocol::parse::<ServerMsg>(t.as_str()) {
            Ok(Parsed::Known(env)) => match env.body {
                ServerMsg::Welcome(w) => {
                    if let Some(secret) = &w.node_secret {
                        if let Err(e) = save_node_secret(&cfg.state_dir, secret) {
                            return SessionEnd::Rejected(NodeError::Rejected { code: "state".into(), message: format!("保存节点身份密钥失败：{e}") });
                        }
                    }
                    *node_id = Some(w.node_id);
                    save_node_id(&cfg.state_dir, w.node_id);
                    break (w.node_id, w.heartbeat_s.max(1), w.resume, w.ack_run_updates);
                }
                ServerMsg::Error(e) if e.code == "unauthorized" || e.code == "version_too_old" => {
                    return SessionEnd::Rejected(NodeError::Rejected { code: e.code, message: e.message });
                }
                ServerMsg::Error(e) => return lost(false, format!("服务端报错 {}：{}", e.code, e.message)),
                _ => continue,
            },
            Ok(Parsed::UnknownType { type_name, .. }) => tracing::warn!(type_name, "握手阶段收到未知消息类型，忽略"),
            Err(e) => tracing::warn!("握手阶段消息格式错误，忽略：{e}"),
        }
    };
    tracing::info!(node_id = ?node_id, heartbeat_s, "已连上平台");

    let mut discarded = HashSet::new();
    for item in resume {
        if item.decision == ResumeDecision::Abort {
            let was = runner.abort(item.run_id);
            outbox.discard_run(item.run_id);
            discarded.insert(item.run_id);
            tracing::warn!(run_id = %item.run_id, was_running = was, "服务端要求放弃这个 run，已停止本地执行");
        }
    }

    if ack_run_updates {
        for update in &outbox.pending {
            if let Err(e) = ws.send(envelope_text(&update.message)).await {
                return lost(true, format!("补发积压消息失败：{e}"));
            }
        }
    } else {
        while let Some(mut update) = outbox.pending.pop_front() {
            if let Some(assign) = update.start.take() {
                update.message = Envelope::new(NodeMsg::RunBlocked(RunBlocked {
                    run_id: assign.run_id, reason: BlockedReason::Link,
                    detail: "服务端不再支持确认任务启动，未开始操作设备".into(),
                }));
            }
            if let Err(e) = ws.send(envelope_text(&update.message)).await {
                outbox.pending.push_front(update);
                return lost(true, format!("补发积压消息失败：{e}"));
            }
        }
    }

    // 连上后先报一次角色摘要（只有摘要，没有 Key）；节点本机还没有角色时上报空列表，服务端据此下发初始模板
    if let Err(e) = ws.send(envelope_text(&Envelope::new(NodeMsg::RolesReport(roles.report())))).await {
        return lost(true, format!("上报角色摘要失败：{e}"));
    }

    let period = Duration::from_secs(u64::from(heartbeat_s));
    let mut last_receipt = tokio::time::Instant::now();
    let mut tick = tokio::time::interval_at(last_receipt + period, period);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(last_receipt + Duration::from_secs(30)), if ack_run_updates && !outbox.pending.is_empty() => {
                return lost(true, "运行上报确认超时".into());
            }
            _ = tick.tick() => {
                let hb = NodeMsg::Heartbeat(Heartbeat { devices: devices.list().await });
                if let Err(e) = ws.send(envelope_text(&Envelope::new(hb))).await {
                    return lost(true, format!("发送心跳失败：{e}"));
                }
            }
            Some(m) = outbox.rx.recv() => {
                let run_id = m.tracked_run();
                if run_id.is_some_and(|id| discarded.contains(&id)) {
                    continue;
                }
                let message = Envelope::new(m);
                let result = ws.send(envelope_text(&message)).await;
                if run_id.is_some() && (ack_run_updates || result.is_err()) {
                    if outbox.pending.is_empty() {
                        last_receipt = tokio::time::Instant::now();
                    }
                    outbox.pending.push_back(PendingUpdate { message, start: None });
                }
                if let Err(e) = result {
                    return lost(true, format!("发送消息失败：{e}"));
                }
            }
            msg = ws.next() => match msg {
                None => return lost(true, "服务端关闭了连接".into()),
                Some(Err(e)) => return lost(true, format!("连接出错：{e}")),
                Some(Ok(Message::Close(_))) => return lost(true, "服务端关闭了连接".into()),
                Some(Ok(Message::Text(t))) => match protocol::parse::<ServerMsg>(t.as_str()) {
                    Ok(Parsed::Known(env)) => match env.body {
                        ServerMsg::RunAssign(a) => {
                            let ack = Envelope::new(NodeMsg::Ack { ack_id: env.id });
                            if let Err(e) = ws.send(envelope_text(&ack)).await {
                                return lost(true, format!("发送确认失败：{e}"));
                            }
                            if runner.running().contains(&a.run_id) || outbox.pending.iter().any(|update| update.message.body.tracked_run() == Some(a.run_id)) {
                                continue;
                            }
                            discarded.remove(&a.run_id);
                            if ack_run_updates {
                                let message = Envelope::new(NodeMsg::RunStarted { run_id: a.run_id });
                                let frame = envelope_text(&message);
                                if outbox.pending.is_empty() {
                                    last_receipt = tokio::time::Instant::now();
                                }
                                outbox.pending.push_back(PendingUpdate { message, start: Some(a) });
                                if let Err(e) = ws.send(frame).await {
                                    return lost(true, format!("发送启动申请失败：{e}"));
                                }
                            } else {
                                runner.spawn(authenticated_node, a);
                            }
                        }
                        ServerMsg::RunCancel { run_id } => {
                            let was = runner.abort(run_id);
                            outbox.discard_run(run_id);
                            discarded.insert(run_id);
                            tracing::warn!(%run_id, was_running = was, "收到取消，已停止本地执行");
                        }
                        ServerMsg::Ack { ack_id } => {
                            if let Some(update) = outbox.receipt(ack_id) {
                                last_receipt = tokio::time::Instant::now();
                                if let Some(assign) = update.start {
                                    tracing::info!(run_id = %assign.run_id, case = %assign.case.code, "服务端已确认，开始执行");
                                    runner.spawn_confirmed(authenticated_node, assign);
                                }
                            }
                        }
                        ServerMsg::Error(e) => {
                            if e.ack_id.and_then(|id| outbox.receipt(id)).is_some() {
                                last_receipt = tokio::time::Instant::now();
                            }
                            tracing::warn!(code = e.code, "服务端报错：{}", e.message);
                        }
                        ServerMsg::SetRoles(set) => {
                            // 日志只记个数；RoleSet 的 Debug 已隐藏 Key
                            match roles.set(set.roles) {
                                Ok(()) => tracing::info!("已更新本机角色配置"),
                                Err(e) => tracing::warn!("保存角色配置失败：{e}"),
                            }
                            if let Err(e) = ws.send(envelope_text(&Envelope::new(NodeMsg::RolesReport(roles.report())))).await {
                                return lost(true, format!("上报角色摘要失败：{e}"));
                            }
                        }
                        ServerMsg::SetParams(params) => {
                            let applied = params::apply(params);
                            tracing::info!(device_retry = applied.device_retry, stuck_n = applied.stuck_n, stuck_keep = applied.stuck_keep, "已更新运行参数");
                        }
                        ServerMsg::CheckRole { role_id } => {
                            if let Some((source, model)) = roles.begin_check(&role_id) {
                                if let Err(e) = ws.send(envelope_text(&Envelope::new(NodeMsg::RolesReport(roles.report())))).await {
                                    return lost(true, format!("上报角色摘要失败：{e}"));
                                }
                                let (roles, tx) = (roles.clone(), role_tx.clone());
                                tokio::spawn(async move {
                                    let result = roles.run_probe(source, model.clone()).await;
                                    roles.finish_check(&role_id, source, &model, result);
                                    // 连接若已断开，发不出去也没关系：重连后会重新上报
                                    let _ = tx.send(NodeMsg::RolesReport(roles.report()));
                                });
                            }
                        }
                        ServerMsg::ScanApp { source } => {
                            // 只跑本地命令（版本、登录状态、模型列表），不调模型；在后台跑，不堵住这条连接
                            let tx = role_tx.clone();
                            tokio::spawn(async move {
                                let scan = agent::apps::scan(source).await;
                                let _ = tx.send(NodeMsg::AppsReport(protocol::AppsReport { apps: vec![scan] }));
                            });
                        }
                        ServerMsg::AdminAsk(ask) => {
                            // 在后台回答，不堵住这条连接。用本机的管理员角色、本机的模型和额度（规则 9）；没有这个角色就如实说没有
                            let (roles, tx) = (roles.clone(), role_tx.clone());
                            tokio::spawn(async move {
                                let _ = tx.send(NodeMsg::AdminAnswer(admin_answer(&roles, ask).await));
                            });
                        }
                        ServerMsg::Ping | ServerMsg::Welcome(_) => {}
                        // 人工处理结果、画面订阅还没接入：明确记一笔，不能假装收到了
                        other => tracing::warn!("这类消息还不处理，已忽略：{other:?}"),
                    },
                    Ok(Parsed::UnknownType { type_name, .. }) => tracing::warn!(type_name, "收到未知消息类型，忽略"),
                    Err(e) => tracing::warn!("消息格式错误，忽略：{e}"),
                },
                Some(Ok(_)) => {}
            }
        }
    }
}

/// 管理员回答一句话：找本机的管理员角色（请求指定了就用它，没有就用第一个；一个都没有就如实说没有，不换别的角色）。
/// 只是说话：没有任何工具，提议由服务端核对、人确认后才会执行。
async fn admin_answer(roles: &roles::RoleStore, ask: protocol::AdminAsk) -> protocol::AdminAnswer {
    let fail = |error: String| protocol::AdminAnswer { ask_id: ask.ask_id, ok: false, reply: String::new(), proposals: vec![], error };
    let configs = roles.configs();
    let Some(role) = configs.iter().find(|r| r.slot == protocol::RoleSlot::Planner && ask.role_id.as_deref().is_none_or(|id| r.id == id)) else {
        return fail("这个节点没有配置「管理员」角色（或指定的角色不存在），请先在「角色与模型」页配置并检测".into());
    };
    match agent::admin::answer(role.source, Some(role.default.model.clone()), role.default.effort.clone(), &ask).await {
        Ok((reply, proposals, dropped)) => {
            let reply = if dropped > 0 { format!("{reply}\n\n（管理员有 {dropped} 条提议格式不对或不在允许的范围内，已忽略。）") } else { reply };
            protocol::AdminAnswer { ask_id: ask.ask_id, ok: true, reply, proposals, error: String::new() }
        }
        Err((reason, quota)) => {
            if quota {
                tracing::warn!("管理员角色额度用完，已停止，不换计费方式");
            }
            fail(reason)
        }
    }
}

/// 常驻运行：断线自动重连；被服务端拒绝（令牌无效 / 版本过低）时返回错误，不再重试。
/// `executor` 负责执行领到的 run；没有执行能力的节点用 [`NoExecutor`]。
pub async fn run<D: Devices, E: RunExecutor>(cfg: NodeConfig, devices: D, executor: E) -> Result<(), NodeError> {
    let mut node_id = load_node_id(&cfg.state_dir);
    let (tx, rx) = mpsc::unbounded_channel();
    let role_tx = tx.clone();
    let runner = Runner::new(executor, tx);
    let roles = Arc::new(roles::RoleStore::open(&cfg.state_dir, roles::real_probe()));
    let mut outbox = Outbox { rx, pending: VecDeque::new() };
    let mut attempt: u32 = 0;
    loop {
        match session(&cfg, &devices, &runner, &mut outbox, &mut node_id, &roles, &role_tx).await {
            SessionEnd::Rejected(e) => return Err(e),
            SessionEnd::Lost { connected, reason } => {
                if connected {
                    attempt = 0;
                }
                let wait = backoff(cfg.backoff_base, cfg.backoff_cap, attempt, rand::random::<f64>());
                attempt = attempt.saturating_add(1);
                tracing::warn!("{reason}；{:.1} 秒后重连", wait.as_secs_f64());
                tokio::time::sleep(wait).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Two;
    impl Devices for Two {
        async fn list(&self) -> Vec<DeviceInfo> {
            ["A", "B"].iter().map(|s| DeviceInfo { serial: s.to_string(), platform: "android".into(), model: None, os_version: None, screen_w: None, screen_h: None, status: protocol::DeviceStatus::Idle }).collect()
        }
    }

    #[tokio::test]
    async fn 设备白名单只放行指定设备_为空不限制() {
        let only = Only { inner: Two, serials: vec!["B".into()] };
        assert_eq!(only.list().await.iter().map(|d| d.serial.as_str()).collect::<Vec<_>>(), ["B"]);
        assert_eq!(Only { inner: Two, serials: vec![] }.list().await.len(), 2);
        assert!(Only { inner: Two, serials: vec!["不存在".into()] }.list().await.is_empty(), "白名单里没有的设备一台都不暴露");
    }

    const B: Duration = Duration::from_secs(1);
    const C: Duration = Duration::from_secs(30);

    #[test]
    fn 退避按倍数增长并封顶() {
        // jitter=0.5 时系数正好是 1.0
        let w = |a| backoff(B, C, a, 0.5).as_secs_f64();
        assert_eq!([w(0), w(1), w(2), w(3), w(4)], [1.0, 2.0, 4.0, 8.0, 16.0]);
        assert_eq!(w(5), 30.0, "32 秒应被封顶到 30 秒");
        assert_eq!(w(50), 30.0, "次数很大也不能溢出");
    }

    #[test]
    fn 抖动范围是正负百分之二十() {
        assert_eq!(backoff(B, C, 2, 0.0).as_secs_f64(), 4.0 * 0.8);
        assert!((backoff(B, C, 2, 1.0).as_secs_f64() - 4.0 * 1.2).abs() < 1e-9);
        assert_eq!(backoff(B, C, 2, 9.9), backoff(B, C, 2, 1.0), "jitter 超范围要夹住");
    }

    #[test]
    fn node_id读写() {
        let dir = std::env::temp_dir().join(format!("node_id_test_{}", Uuid::new_v4()));
        assert_eq!(load_node_id(&dir), None);
        let id = Uuid::new_v4();
        save_node_id(&dir, id);
        assert_eq!(load_node_id(&dir), Some(id));
        std::fs::write(node_id_path(&dir), "不是 uuid").unwrap();
        assert_eq!(load_node_id(&dir), None, "文件内容坏了要当作没有，而不是报错退出");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn 没有管理员角色时如实回答没有_不换别的角色() {
        let dir = std::env::temp_dir().join(format!("node_admin_{}", Uuid::new_v4()));
        let store = roles::RoleStore::open(&dir, roles::real_probe());
        // 只配了一个判定员：不能拿来当管理员
        store
            .set(vec![protocol::RoleSet {
                config: protocol::RoleConfig {
                    id: "j".into(),
                    slot: protocol::RoleSlot::Judge,
                    name: "判定员".into(),
                    source: ModelSource::ClaudeCli,
                    default: protocol::RoleDefault { model: "m".into(), effort: None },
                    models: vec![protocol::RoleModelOption { model: "m".into(), efforts: vec![] }],
                },
                api_key: None,
            }])
            .unwrap();
        let ask = protocol::AdminAsk { ask_id: Uuid::new_v4(), role_id: None, context: "现状".into(), history: vec![], message: "你好".into() };
        let answer = admin_answer(&store, ask.clone()).await;
        assert!(!answer.ok && answer.proposals.is_empty());
        assert!(answer.error.contains("没有配置「管理员」角色"), "{}", answer.error);
        assert_eq!(answer.ask_id, ask.ask_id);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
