//! 平台服务端：节点接入、任务派发与 PostgreSQL 持久化。
//!
//! 服务默认只监听本机（`PLATFORM_ALLOW_LAN=1` 才允许放开局域网，见 main.rs）。账号明文只经节点令牌认证的一次性接口交给执行节点。

pub mod accounts;
mod api;
pub mod cases;
mod evidence_sign;
mod files;
pub mod recording_retention;
mod reviews;
pub mod users;
pub mod runs;
pub mod scripts;
mod scripts_api;
#[cfg(feature = "test-support")]
pub mod test_support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use protocol::{
    Caps, DeviceInfo, DeviceStatus, Envelope, Hello, LeaseCredentials, NodeMsg, Parsed, ProtoError, ResumeItem, RunAssign, RunBlocked, ServerMsg,
    Welcome, PROTOCOL_VERSION,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Config {
    /// 节点令牌。P0 是单个共享令牌，从环境变量读，**代码里不写默认值**；之后改为每节点一个、只存哈希
    pub node_token: String,
    /// 连接后必须在这个时间内发 `hello`
    pub hello_timeout: Duration,
    /// 下发给节点的心跳间隔（秒）；超过 `3 × heartbeat_s` 没有任何消息判离线
    pub heartbeat_s: u32,
    /// 节点掉线后，执行中的 run 保留多久等它重连续跑；超过就判受阻。期间这些 run 仍占着设备
    pub lost_grace: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeView {
    pub node_id: Uuid,
    pub online: bool,
    pub caps: Caps,
    pub devices: Vec<DeviceInfo>,
    pub last_seen: DateTime<Utc>,
}

/// 一个节点当前的连接：服务端要主动给节点发消息（派发、取消）时，往发件箱里放。
struct Conn {
    conn_id: Uuid,
    tx: mpsc::UnboundedSender<ServerMsg>,
}

/// 派发的轮询间隔：节点连着时，每隔这么久看一次有没有排队的 run 和空闲设备。
pub const DISPATCH_INTERVAL: Duration = Duration::from_millis(500);

struct Inner {
    cfg: Config,
    pool: PgPool,
    accounts: Option<accounts::AccountPool>,
    review_auth: reviews::ReviewAuth,
    /// 每个节点当前有效的连接（连接 id + 发件箱）。同一节点重连时，旧连接的收尾不能把新连接标成离线。
    /// 只在本进程内有效（单实例）；多实例部署时要换成共享的租约。
    conns: Mutex<HashMap<Uuid, Conn>>,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

fn status_str(s: DeviceStatus) -> &'static str {
    match s {
        DeviceStatus::Idle => "idle",
        DeviceStatus::Busy => "busy",
        DeviceStatus::Offline => "offline",
    }
}

fn status_from(s: &str) -> DeviceStatus {
    match s {
        "idle" => DeviceStatus::Idle,
        "busy" => DeviceStatus::Busy,
        _ => DeviceStatus::Offline,
    }
}

fn run_err(e: runs::RunError) -> sqlx::Error {
    match e {
        runs::RunError::Db(d) => d,
        other => sqlx::Error::Protocol(other.to_string()),
    }
}

/// 只跑迁移，不碰节点状态。命令行工具（如导入用例）用它：
/// 不能用 `AppState::init`，它会把所有节点置离线，服务运行时导入会把在线节点误伤。
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations").run(pool).await
}

impl AppState {
    /// 跑迁移，并把所有节点和设备置离线：服务刚启动时还没有任何连接，
    /// 数据库里上次留下的「在线」不可信。
    pub async fn init(cfg: Config, pool: PgPool) -> Result<Self, sqlx::Error> {
        Self::init_with_accounts(cfg, pool, accounts::AccountPool::configured()).await
    }

    pub(crate) async fn init_with_accounts(cfg: Config, pool: PgPool, accounts: Option<accounts::AccountPool>) -> Result<Self, sqlx::Error> {
        migrate(&pool).await?;
        let mut tx = pool.begin().await?;
        sqlx::query("update nodes set online = false").execute(&mut *tx).await?;
        sqlx::query("update devices set status = 'offline'").execute(&mut *tx).await?;
        // 服务进程重启（或崩溃）时没有走「节点离线」那条路径：库里还停在执行中的 run 一并转 node_lost，
        // 否则节点重连后会被当成「没有这个 run」而被放弃
        runs::all_nodes_lost(&mut tx).await.map_err(run_err)?;
        tx.commit().await?;
        Ok(Self(Arc::new(Inner {
            cfg,
            pool,
            accounts,
            review_auth: reviews::ReviewAuth::from_env(),
            conns: Mutex::new(HashMap::new()),
        })))
    }

    /// 把节点上报的设备同步进库：上报的 upsert；这次没报的（拔了）置离线。
    /// 必须在调用方的事务里执行：节点和设备要一起落库，否则读取方会看到「节点在线但没有设备」。
    async fn sync_devices(conn: &mut sqlx::PgConnection, node_id: Uuid, devices: &[DeviceInfo]) -> Result<(), sqlx::Error> {
        for d in devices {
            sqlx::query(
                "insert into devices (node_id, serial, platform, model, os_version, screen_w, screen_h, status, last_seen)
                 values ($1,$2,$3,$4,$5,$6,$7,$8, now())
                 on conflict (node_id, serial) do update set
                   platform = excluded.platform, model = excluded.model, os_version = excluded.os_version,
                   screen_w = excluded.screen_w, screen_h = excluded.screen_h,
                   -- 有活跃 run 的设备由服务端说了算（busy）：节点心跳里的「空闲」不能把设备锁覆盖掉
                   status = case
                     when excluded.status = 'offline' then 'offline'
                     when exists (select 1 from runs r where r.device_id = devices.id and r.status in ('assigned','running','judging','node_lost')) then 'busy'
                     else excluded.status end,
                   last_seen = now()",
            )
            .bind(node_id)
            .bind(&d.serial)
            .bind(&d.platform)
            .bind(&d.model)
            .bind(&d.os_version)
            .bind(d.screen_w.map(|v| v as i32))
            .bind(d.screen_h.map(|v| v as i32))
            .bind(status_str(d.status))
            .execute(&mut *conn)
            .await?;
        }
        let serials: Vec<&str> = devices.iter().map(|d| d.serial.as_str()).collect();
        sqlx::query("update devices set status = 'offline' where node_id = $1 and not (serial = any($2))")
            .bind(node_id)
            .bind(&serials)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    async fn register(&self, hello: &Hello, tx_out: mpsc::UnboundedSender<ServerMsg>) -> Result<(Uuid, Uuid, Vec<ResumeItem>, Option<String>), RegisterError> {
        let node_id = hello.node_id.unwrap_or_else(Uuid::new_v4);
        let conn_id = Uuid::new_v4();
        let caps = serde_json::to_value(&hello.caps).expect("Caps 可序列化");
        let mut conns = self.0.conns.lock().await;
        let mut tx = self.0.pool.begin().await?;
        // 节点身份：已发过密钥的节点必须带对密钥；没有密钥的（新节点、老节点）由服务端新发
        let stored: Option<Option<String>> = sqlx::query_scalar("select secret_hash from nodes where id = $1 for update")
            .bind(node_id)
            .fetch_optional(&mut *tx)
            .await?;
        let issued = match stored {
            Some(Some(hash)) => {
                if !hello.node_secret.as_deref().is_some_and(|s| secret_matches(s, &hash)) {
                    return Err(RegisterError::Unauthorized);
                }
                None
            }
            Some(None) | None => Some(new_node_secret()),
        };
        // 节点属主：带了个人令牌就必须有效；节点已有属主时必须是同一个人；还没有属主就绑定
        let presented = match hello.user_token.as_deref() {
            Some(token) => Some(users::user_by_token(&mut *tx, token).await?.ok_or(RegisterError::Unauthorized)?),
            None => None,
        };
        let current_owner: Option<Uuid> = sqlx::query_scalar("select owner_user_id from nodes where id = $1").bind(node_id).fetch_optional(&mut *tx).await?.flatten();
        let new_owner = match (current_owner, presented) {
            (Some(owner), Some(who)) if owner != who => return Err(RegisterError::Unauthorized),
            (None, Some(who)) => Some(who),
            _ => None,
        };
        sqlx::query(
            "insert into nodes (id, caps, online, last_seen, secret_hash, owner_user_id) values ($1, $2, true, now(), $3, $4)
             on conflict (id) do update set caps = excluded.caps, online = true, last_seen = now(),
                 secret_hash = coalesce($3, nodes.secret_hash),
                 owner_user_id = coalesce($4, nodes.owner_user_id)",
        )
        .bind(node_id)
        .bind(caps)
        .bind(issued.as_deref().map(secret_hash))
        .bind(new_owner)
        .execute(&mut *tx)
        .await?;
        if conns.contains_key(&node_id) {
            runs::node_lost(&mut tx, node_id).await.map_err(run_err)?;
        }
        Self::sync_devices(&mut tx, node_id, &hello.devices).await?;
        // 节点声称仍在跑的 run：在宽限期内的继续，其它放弃；服务端以为在跑、节点却没有的（节点重启丢了进度）判受阻
        let resume = runs::decide_resume(&mut tx, node_id, &hello.running_runs, self.0.cfg.lost_grace).await.map_err(run_err)?;
        accounts::heartbeat(&mut tx, node_id, accounts::DEFAULT_TTL).await?;
        tx.commit().await?;
        conns.insert(node_id, Conn { conn_id, tx: tx_out });
        Ok((node_id, conn_id, resume, issued))
    }

    async fn is_current(&self, node_id: Uuid, conn_id: Uuid) -> bool {
        self.0.conns.lock().await.get(&node_id).map(|c| c.conn_id) == Some(conn_id)
    }

    async fn touch(&self, node_id: Uuid, conn_id: Uuid, devices: Option<&[DeviceInfo]>) -> Result<(), sqlx::Error> {
        if !self.is_current(node_id, conn_id).await {
            return Ok(());
        }
        let mut tx = self.0.pool.begin().await?;
        sqlx::query("update nodes set last_seen = now() where id = $1").bind(node_id).execute(&mut *tx).await?;
        if let Some(d) = devices {
            Self::sync_devices(&mut tx, node_id, d).await?;
            accounts::heartbeat(&mut tx, node_id, accounts::DEFAULT_TTL).await?;
        }
        tx.commit().await
    }

    /// 连接结束：节点离线，其下设备全部置离线。只处理自己这条连接，避免误伤重连后的新连接。
    async fn mark_offline(&self, node_id: Uuid, conn_id: Uuid) -> Result<(), sqlx::Error> {
        let mut conns = self.0.conns.lock().await;
        if conns.get(&node_id).map(|c| c.conn_id) != Some(conn_id) {
            return Ok(());
        }
        // 节点和它的设备必须在一个事务里一起置离线：否则外部会短暂看到「节点已离线、设备还空闲」，
        // 以后派任务时可能把任务派给一台其实已经离线的手机。
        let mut tx = self.0.pool.begin().await?;
        sqlx::query("update nodes set online = false where id = $1").bind(node_id).execute(&mut *tx).await?;
        sqlx::query("update devices set status = 'offline' where node_id = $1").bind(node_id).execute(&mut *tx).await?;
        // 执行中的 run 转 node_lost（不直接判失败）；还没开始的退回排队
        runs::node_lost(&mut tx, node_id).await.map_err(run_err)?;
        tx.commit().await?;
        conns.remove(&node_id);
        Ok(())
    }

    /// 给节点发一条消息；节点不在线（没有有效连接）返回 `false`。
    pub async fn send_to(&self, node_id: Uuid, msg: ServerMsg) -> bool {
        match self.0.conns.lock().await.get(&node_id) {
            Some(c) => c.tx.send(msg).is_ok(),
            None => false,
        }
    }

    /// 取消一个 run：先在库里取消（释放设备），再通知它所在的节点停下来。节点不在线时库里照样取消。
    pub async fn cancel_run(&self, run_id: Uuid) -> Result<(), runs::RunError> {
        let node: Option<Uuid> = sqlx::query_scalar("select node_id from runs where id = $1").bind(run_id).fetch_optional(&self.0.pool).await?;
        runs::cancel(&self.0.pool, run_id).await?;
        if let Some(n) = node {
            self.send_to(n, ServerMsg::RunCancel { run_id }).await;
        }
        Ok(())
    }

    /// 后台定时处理掉线 run 与到期租约。进程里只需要起一个。
    pub fn spawn_housekeeping(&self, every: Duration) -> tokio::task::JoinHandle<()> {
        let (pool, grace) = (self.0.pool.clone(), self.0.cfg.lost_grace);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                match runs::expire_node_lost(&pool, grace).await {
                    Ok(n) if n > 0 => tracing::warn!("{n} 个 run 因节点掉线超过宽限期，已判受阻"),
                    Ok(_) => {}
                    Err(e) => tracing::error!("清理掉线 run 失败：{e}"),
                }
                match accounts::expire_leases(&pool, grace).await {
                    Ok(n) if n > 0 => tracing::warn!("已回收 {n} 个到期账号租约"),
                    Ok(_) => {}
                    Err(_) => tracing::error!("回收到期账号租约时数据库操作失败"),
                }
            }
        })
    }

    pub async fn claim_next(&self, node_id: Uuid) -> Result<Option<runs::Claimed>, runs::RunError> {
        runs::claim_next_with_accounts(&self.0.pool, node_id, self.0.accounts.as_ref()).await
    }

    /// 给这个节点派活：只要有排队的 run 和空闲的设备就一直领。领到就发 `run_assign`。
    /// 发送失败（连接已断）也没关系：run 保持 assigned，节点离线时会退回排队。
    async fn dispatch(&self, node_id: Uuid, conn_id: Uuid) {
        loop {
            let conns = self.0.conns.lock().await;
            let Some(conn) = conns.get(&node_id).filter(|conn| conn.conn_id == conn_id) else { break };
            match self.claim_next(node_id).await {
                Ok(Some(c)) => {
                    let limits = limits_for(&c.case);
                    let assign = RunAssign {
                        run_id: c.run_id, case: c.case, device_serial: c.device_serial, executor: c.executor, judge: c.judge,
                        lease_id: c.lease_id, needs_cleanup: c.needs_cleanup, limits,
                    };
                    tracing::info!(%node_id, run_id = %c.run_id, "派发 run");
                    if conn.tx.send(ServerMsg::RunAssign(assign)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(%node_id, "派发失败：{e}");
                    break;
                }
            }
        }
    }

    async fn handle_node_msg(&self, node_id: Uuid, conn_id: Uuid, msg: Envelope<NodeMsg>) -> Result<Option<ServerMsg>, ()> {
        let pool = &self.0.pool;
        let ack_id = msg.body.tracked_run().map(|_| msg.id);
        let mut freed = false; // 这条消息是否可能释放了设备
        let result: Result<(), runs::RunError> = match msg.body {
            NodeMsg::Heartbeat(h) | NodeMsg::DeviceUpdate(h) => self.touch(node_id, conn_id, Some(&h.devices)).await.map_err(Into::into),
            NodeMsg::Hello(_) => {
                tracing::warn!(%node_id, "已连接后又收到 hello，忽略");
                Ok(())
            }
            NodeMsg::RunStarted { run_id } => {
                let conns = self.0.conns.lock().await;
                if conns.get(&node_id).map(|conn| conn.conn_id) != Some(conn_id) {
                    return Err(());
                }
                runs::mark_started_with_receipt(pool, node_id, run_id, ack_id).await
            }
            NodeMsg::Step(s) => runs::record_step(pool, node_id, &s).await.map(|_| ()),
            NodeMsg::ExecutorDone(d) => runs::executor_done(pool, node_id, &d).await,
            NodeMsg::JudgeDone(j) => {
                freed = true;
                runs::judge_done(pool, node_id, &j).await.map(|o| {
                    if o.overridden {
                        tracing::warn!(%node_id, run_id = %j.run_id, "节点报的最终结果和逐条判定不一致，已以服务端重算为准：{:?}", o.final_result);
                    }
                })
            }
            NodeMsg::RunBlocked(b) => {
                freed = true;
                runs::run_blocked(pool, node_id, &b).await
            }
            NodeMsg::QuotaExhausted(q) => {
                // 额度用完：暂停并如实上报，不自动换计费方式。该 run 判受阻（quota），由人决定怎么办
                tracing::warn!(%node_id, source = ?q.source, "节点报告额度用完");
                match q.run_id {
                    Some(run_id) => {
                        freed = true;
                        runs::run_blocked(pool, node_id, &RunBlocked { run_id, reason: protocol::BlockedReason::Quota, detail: format!("额度用完（{:?}）", q.source) }).await
                    }
                    None => Ok(()),
                }
            }
            NodeMsg::NeedHuman(n) => {
                tracing::warn!(%node_id, run_id = %n.run_id, kind = ?n.kind, "run 需要人工：{}", n.reason);
                Ok(())
            }
            NodeMsg::Ack { .. } | NodeMsg::Frame(_) | NodeMsg::Log(_) => Ok(()),
        };
        let response = match result {
            Ok(()) => ack_id.map(|ack_id| ServerMsg::Ack { ack_id }),
            Err(runs::RunError::Db(e)) if ack_id.is_some() => {
                tracing::warn!(%node_id, "运行上报未能落库，断开连接等待重传：{e}");
                return Err(());
            }
            Err(e) => {
                tracing::warn!(%node_id, "拒绝了节点的一条消息：{e}");
                Some(ServerMsg::Error(ProtoError { code: "run_rejected".into(), message: e.to_string(), ack_id }))
            }
        };
        // 每条消息都算「节点还活着」
        if let Err(e) = self.touch(node_id, conn_id, None).await {
            tracing::error!(%node_id, "刷新存活时间失败：{e}");
        }
        if freed {
            self.dispatch(node_id, conn_id).await; // 设备刚空出来，马上派下一个，不等下一轮轮询
        }
        Ok(response)
    }

    pub fn pool(&self) -> &PgPool {
        &self.0.pool
    }

    pub async fn nodes(&self) -> Result<Vec<NodeView>, sqlx::Error> {
        // 设备和节点是两条查询，必须在同一个快照里读：否则读设备时事务还没提交、读节点时已提交，
        // 会拼出「节点离线、设备还空闲」的不一致画面（写入侧的事务只保证写是原子的，读也要一致）。
        let mut tx = self.0.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read read only").execute(&mut *tx).await?;
        let mut devices: HashMap<Uuid, Vec<DeviceInfo>> = HashMap::new();
        for r in sqlx::query(
            "select node_id, serial, platform, model, os_version, screen_w, screen_h, status
             from devices order by serial",
        )
        .fetch_all(&mut *tx)
        .await?
        {
            devices.entry(r.get("node_id")).or_default().push(DeviceInfo {
                serial: r.get("serial"),
                platform: r.get("platform"),
                model: r.get("model"),
                os_version: r.get("os_version"),
                screen_w: r.get::<Option<i32>, _>("screen_w").map(|v| v as u32),
                screen_h: r.get::<Option<i32>, _>("screen_h").map(|v| v as u32),
                status: status_from(r.get("status")),
            });
        }
        let mut out = Vec::new();
        for r in sqlx::query("select id, caps, online, last_seen from nodes order by id").fetch_all(&mut *tx).await? {
            let id: Uuid = r.get("id");
            let caps: serde_json::Value = r.get("caps");
            out.push(NodeView {
                node_id: id,
                online: r.get("online"),
                caps: serde_json::from_value(caps).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                devices: devices.remove(&id).unwrap_or_default(),
                last_seen: r.get("last_seen"),
            });
        }
        tx.commit().await?;
        Ok(out)
    }
}

/// 一次执行的限制：最大步数 = 用例步骤数 × 3（至少 10），整体时限 900 秒。规则来自技术验证。
pub fn limits_for(case: &protocol::CaseSpec) -> protocol::Limits {
    protocol::Limits { max_steps: (case.steps.len() as u32 * 3).max(10), timeout_s: 900 }
}

pub fn app(state: AppState) -> Router {
    app_with_paths(state, api::Paths::from_env(), false)
}

/// 局域网模式：写接口的 Host / Origin 校验额外放行私有网段 IP（仍然拒绝域名）。没有登录鉴权，调用方必须自己明确打开。
pub fn app_lan(state: AppState) -> Router {
    app_with_paths(state, api::Paths::from_env(), true)
}

fn app_with_paths(state: AppState, paths: api::Paths, lan: bool) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/nodes", get(list_nodes))
        .route("/api/projects", get(api_projects))
        .route("/api/projects/{project}/cases", get(api_cases))
        .route("/api/projects/{project}/cases/{code}", get(api_case))
        .route("/api/leases/{lease_id}/credentials", get(api_lease_credentials).head(|| async { StatusCode::METHOD_NOT_ALLOWED }))
        .route("/ws/node", get(ws_node))
        .with_state(state.clone())
        .merge(api::routes(state, paths))
        .layer(axum::middleware::from_fn(move |req, next| api::browser_write_guard(req, next, lan)))
}

/// 注册失败：数据库出错，或节点身份密钥不对。
enum RegisterError {
    Db(sqlx::Error),
    Unauthorized,
}

impl From<sqlx::Error> for RegisterError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// 新的节点身份密钥：两个随机 UUID v4 拼接（约 244 位随机）。
fn new_node_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// 库里只存摘要。
fn secret_hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn secret_matches(provided: &str, stored_hash: &str) -> bool {
    let provided = Sha256::digest(secret_hash(provided).as_bytes());
    let stored = Sha256::digest(stored_hash.as_bytes());
    bool::from(provided[..].ct_eq(&stored[..]))
}

fn node_token_matches(provided: &str, expected: &str) -> bool {
    // 对定长摘要比较，避免直接比较令牌时在长度不同时提前返回。
    let provided = Sha256::digest(provided.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    bool::from(provided[..].ct_eq(&expected[..]))
}

async fn api_lease_credentials(State(st): State<AppState>, Path(lease): Path<String>, headers: HeaderMap) -> impl IntoResponse {
    let result = lease_credentials(&st, &lease, &headers).await;
    ([(header::CACHE_CONTROL, "no-store")], result)
}

async fn lease_credentials(st: &AppState, lease: &str, headers: &HeaderMap) -> Result<Json<LeaseCredentials>, StatusCode> {
    let node = node_identity(st, headers).await?;
    let lease = lease.parse().map_err(|_| StatusCode::GONE)?;
    let key = st.0.accounts.as_ref().map(|a| &a.key);
    use accounts::CredentialError;
    accounts::take_credentials(st.pool(), key, lease, node).await.map(Json).map_err(|error| match error {
        CredentialError::Forbidden => StatusCode::FORBIDDEN,
        CredentialError::Gone => StatusCode::GONE,
        CredentialError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        CredentialError::Cipher => {
            tracing::error!("解密账号凭证失败");
            StatusCode::INTERNAL_SERVER_ERROR
        }
        CredentialError::Database => {
            tracing::error!("读取账号租约时数据库操作失败");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })
}

/// 节点 HTTP 请求的身份：共享令牌 + 自报的 `X-Node-Id`，再校验 `X-Node-Secret`。
/// 库里该节点已有密钥时，缺失或不符返回 403；没有密钥的老节点暂按旧规则放行，
/// 它下次通过 WebSocket 重连时服务端会补发密钥，之后就走严格校验。
pub(crate) async fn node_identity(st: &AppState, headers: &HeaderMap) -> Result<Uuid, StatusCode> {
    let node = node_identity_claim(st, headers)?;
    let stored: Option<Option<String>> = sqlx::query_scalar("select secret_hash from nodes where id = $1")
        .bind(node)
        .fetch_optional(st.pool())
        .await
        .map_err(|e| {
            tracing::error!("查询节点身份失败：{e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if let Some(Some(hash)) = stored {
        let secret = headers.get("x-node-secret").and_then(|h| h.to_str().ok());
        if !secret.is_some_and(|s| secret_matches(s, &hash)) {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    Ok(node)
}

fn node_identity_claim(st: &AppState, headers: &HeaderMap) -> Result<Uuid, StatusCode> {
    let token = headers.get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, token)| token);
    if !token.is_some_and(|token| !token.is_empty() && node_token_matches(token, &st.0.cfg.node_token)) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    headers.get("x-node-id").and_then(|h| h.to_str().ok()).and_then(|h| h.parse().ok()).ok_or(StatusCode::FORBIDDEN)
}

async fn list_nodes(State(st): State<AppState>) -> Result<Json<Vec<NodeView>>, (StatusCode, String)> {
    st.nodes().await.map(Json).map_err(|e| {
        // 原始错误写日志，不把数据库细节返回给调用方
        tracing::error!("查询节点失败：{e}");
        (StatusCode::INTERNAL_SERVER_ERROR, "查询节点失败".into())
    })
}

fn internal(what: &str, e: sqlx::Error) -> (StatusCode, String) {
    // 原始错误写日志，不把数据库细节返回给调用方
    tracing::error!("{what}：{e}");
    (StatusCode::INTERNAL_SERVER_ERROR, what.to_string())
}

async fn api_projects(State(st): State<AppState>) -> Result<Json<Vec<cases::ProjectSummary>>, (StatusCode, String)> {
    cases::projects(st.pool()).await.map(Json).map_err(|e| internal("查询项目失败", e))
}

async fn api_cases(State(st): State<AppState>, axum::extract::Path(project): axum::extract::Path<String>) -> Result<Json<Vec<cases::CaseSummary>>, (StatusCode, String)> {
    match cases::list(st.pool(), &project).await.map_err(|e| internal("查询用例失败", e))? {
        Some(v) => Ok(Json(v)),
        None => Err((StatusCode::NOT_FOUND, format!("没有这个项目：{project}"))),
    }
}

async fn api_case(
    State(st): State<AppState>,
    axum::extract::Path((project, code)): axum::extract::Path<(String, String)>,
) -> Result<Json<cases::CaseDetail>, (StatusCode, String)> {
    match cases::detail(st.pool(), &project, &code).await.map_err(|e| internal("查询用例失败", e))? {
        Some(d) => Ok(Json(d)),
        None => Err((StatusCode::NOT_FOUND, format!("没有这个用例：{project}/{code}"))),
    }
}

async fn ws_node(ws: WebSocketUpgrade, State(st): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, st))
}

enum Recv {
    Msg(Parsed<NodeMsg>),
    Bad(String),
    Closed,
}

async fn recv(socket: &mut WebSocket) -> Recv {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(t))) => {
                return match protocol::parse::<NodeMsg>(t.as_str()) {
                    Ok(p) => Recv::Msg(p),
                    Err(e) => Recv::Bad(e.to_string()),
                };
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Recv::Closed,
            Some(Ok(_)) => continue, // ping / pong / binary 忽略
        }
    }
}

async fn send(socket: &mut WebSocket, body: ServerMsg) -> bool {
    match serde_json::to_string(&Envelope::new(body)) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => false,
    }
}

async fn reject(socket: &mut WebSocket, code: &str, message: &str) {
    let _ = send(socket, ServerMsg::Error(ProtoError { code: code.into(), message: message.into(), ack_id: None })).await;
    let _ = socket.send(Message::Close(None)).await;
}

async fn handle_socket(mut socket: WebSocket, st: AppState) {
    let cfg = st.0.cfg.clone();

    // 1. 第一条必须是 hello，且要在限定时间内
    let first = match tokio::time::timeout(cfg.hello_timeout, recv(&mut socket)).await {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!("连接后没有及时发送 hello，断开");
            reject(&mut socket, "hello_timeout", "连接后必须先发送 hello").await;
            return;
        }
    };
    let (hello, ver) = match first {
        Recv::Msg(Parsed::Known(env)) => match env.body {
            NodeMsg::Hello(h) => (h, env.v),
            _ => return reject(&mut socket, "protocol", "第一条消息必须是 hello").await,
        },
        Recv::Closed => return,
        _ => return reject(&mut socket, "protocol", "第一条消息必须是 hello").await,
    };
    if ver < PROTOCOL_VERSION {
        return reject(&mut socket, "version_too_old", "节点协议版本过低，请升级节点").await;
    }
    // 令牌不对：不报具体原因，不留任何注册记录；日志里也不打印令牌
    if !node_token_matches(&hello.token, &cfg.node_token) {
        tracing::warn!("节点令牌无效，拒绝连接");
        return reject(&mut socket, "unauthorized", "节点令牌无效").await;
    }

    // 2. 注册并回 welcome。节点重连时声称仍在跑的 run，由服务端按宽限期决定继续还是放弃（见 runs::decide_resume）
    let (tx_out, mut rx_out) = mpsc::unbounded_channel::<ServerMsg>();
    let (node_id, conn_id, resume, node_secret) = match st.register(&hello, tx_out).await {
        Ok(ids) => ids,
        Err(RegisterError::Unauthorized) => {
            // 不报具体原因，日志里不打印密钥
            tracing::warn!("节点身份密钥不符，拒绝连接");
            return reject(&mut socket, "unauthorized", "节点身份校验失败").await;
        }
        Err(RegisterError::Db(e)) => {
            tracing::error!("节点注册失败：{e}");
            return reject(&mut socket, "internal", "服务端暂时无法注册节点，请稍后重试").await;
        }
    };
    if !send(&mut socket, ServerMsg::Welcome(Welcome { node_id, node_secret, heartbeat_s: cfg.heartbeat_s, resume, ack_run_updates: true })).await {
        let _ = st.mark_offline(node_id, conn_id).await;
        return;
    }
    tracing::info!(%node_id, devices = hello.devices.len(), "节点上线");

    // 3. 主循环：收节点消息、转发待发消息（取消等）、定时派发；超过 3 个心跳间隔没有任何消息判离线
    let limit = Duration::from_secs(u64::from(cfg.heartbeat_s) * 3);
    let mut last_seen = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(DISPATCH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(last_seen + limit) => {
                tracing::warn!(%node_id, "心跳超时，判离线");
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            outgoing = rx_out.recv() => {
                let Some(out) = outgoing else { break; };
                if !send(&mut socket, out).await {
                    break;
                }
            }
            _ = tick.tick() => st.dispatch(node_id, conn_id).await,
            msg = recv(&mut socket) => match msg {
                Recv::Closed => break,
                Recv::Bad(e) => {
                    last_seen = tokio::time::Instant::now();
                    tracing::warn!(%node_id, "消息格式错误，忽略：{e}");
                }
                Recv::Msg(Parsed::UnknownType { type_name, .. }) => {
                    last_seen = tokio::time::Instant::now();
                    tracing::warn!(%node_id, type_name, "收到未知消息类型，忽略");
                }
                Recv::Msg(Parsed::Known(env)) => {
                    last_seen = tokio::time::Instant::now();
                    if !st.is_current(node_id, conn_id).await { break; }
                    match st.handle_node_msg(node_id, conn_id, env).await {
                        Ok(Some(response)) => {
                            if !send(&mut socket, response).await { break; }
                        }
                        Ok(None) => {}
                        Err(()) => break,
                    }
                }
            }
        }
    }
    if let Err(e) = st.mark_offline(node_id, conn_id).await {
        tracing::error!(%node_id, "标记离线失败：{e}");
    }
    tracing::info!(%node_id, "节点离线");
}

#[cfg(test)]
mod credential_auth_tests {
    use super::*;
    use axum::body::to_bytes;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    #[tokio::test]
    async fn 凭证接口先检查令牌和节点头且错误响应为空() {
        let st = AppState(Arc::new(Inner {
            cfg: Config {
                node_token: "synthetic-node-token".into(),
                hello_timeout: Duration::from_secs(1),
                heartbeat_s: 5,
                lost_grace: Duration::from_secs(120),
            },
            pool: PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new()),
            accounts: None,
            review_auth: reviews::ReviewAuth::new(None),
            conns: Mutex::new(HashMap::new()),
        }));
        let lease = Uuid::new_v4().to_string();
        for (authorization, status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer synthetic-node-tokeN"), StatusCode::UNAUTHORIZED),
            (Some("Bearer wrong"), StatusCode::UNAUTHORIZED),
            (Some("Bearer synthetic-node-token"), StatusCode::FORBIDDEN),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(value) = authorization {
                headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            }
            let response = api_lease_credentials(State(st.clone()), Path(lease.clone()), headers).await.into_response();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(to_bytes(response.into_body(), 1024).await.unwrap().is_empty());
        }
    }
}

#[cfg(test)]
mod node_secret_tests {
    use super::{new_node_secret, secret_hash, secret_matches};

    #[test]
    fn 密钥校验只认原密钥() {
        let secret = new_node_secret();
        let hash = secret_hash(&secret);
        assert_eq!(hash.len(), 64, "库里存 SHA-256 十六进制摘要");
        assert!(!hash.contains(&secret), "库里不存明文");
        assert!(secret_matches(&secret, &hash));
        assert!(!secret_matches("别的密钥", &hash));
        assert!(!secret_matches("", &hash));
        assert_ne!(new_node_secret(), secret, "每次生成都不同");
    }
}
