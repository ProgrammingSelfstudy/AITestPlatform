//! 页面使用的任务、run 和模型配置接口。

use std::collections::BTreeMap;
use std::path::{Path as FilePath, PathBuf};

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header, uri::Authority};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use protocol::{CaseExpected, ModelSource, RoleConfig, RoleModel, RoleSlot};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use crate::runs::{self, NewTask, RunError, RunStatus, RunView, TaskCreated};
use crate::users::{self, BrowserUser, PasswordCheck};
use crate::{AppState, evidence_sign, files, internal, reviews};

type ApiError = (StatusCode, String);

#[derive(Clone)]
pub(crate) struct Paths {
    pub models: PathBuf,
    pub data: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        Self {
            models: std::env::var_os("PLATFORM_MODELS")
                .map_or_else(|| "config/models.json".into(), PathBuf::from),
            data: std::env::var_os("PLATFORM_DATA_DIR")
                .map_or_else(|| "data".into(), PathBuf::from),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ApiState {
    pub app: AppState,
    pub paths: Paths,
    pub evidence_signer: &'static evidence_sign::Signer,
}

pub(crate) fn routes(app: AppState, paths: Paths) -> Router {
    let authorize_write = axum::middleware::from_fn_with_state(app.clone(), reviews::authorize);
    Router::new()
        .route("/api/models", get(models))
        .route("/api/configs", get(crate::configs::api_list_types))
        .route("/api/configs/{code}", get(crate::configs::api_type_detail))
        .route("/api/projects/{project}/modules", post(crate::modules::api_create_module).route_layer(authorize_write.clone()).get(crate::modules::api_list))
        .route(
            "/api/projects/{project}/casegen-requests",
            post(crate::modules::api_create_request)
                .route_layer(authorize_write.clone())
                // 截图用 base64 放在 JSON 里：最多 6 张 × 3MB，再加文档，放宽到 32MB
                .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        .route("/api/casegen-requests/{id}", get(crate::modules::api_request_detail))
        .route("/api/node/casegen-requests/claim", put(crate::modules::api_claim))
        .route("/api/casegen-requests/{id}/review", put(crate::modules::api_review).layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)))
        .route("/api/modules/{id}/cases", get(crate::modules::api_module_cases))
        .route("/api/modules/{id}/cases/{code}/decision", post(crate::modules::api_decide).route_layer(authorize_write.clone()))
        .route("/api/modules/{id}/approve-reviewed", post(crate::modules::api_approve_reviewed).route_layer(authorize_write.clone()))
        .route("/api/casegen-requests/{id}/result", put(crate::modules::api_finish).layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024)))
        .route("/api/nodes/{id}/roles", post(set_node_roles).route_layer(authorize_write.clone()).get(node_roles))
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/admin/chat", get(crate::admin::api_chat))
        .route("/api/admin/messages", post(crate::admin::api_send).route_layer(authorize_write.clone()))
        .route("/api/admin/proposals/{id}/confirm", post(crate::admin::api_confirm).route_layer(authorize_write.clone()))
        .route("/api/admin/proposals/{id}/dismiss", post(crate::admin::api_dismiss))
        .route("/api/nodes/{id}/apps", get(node_apps))
        .route("/api/nodes/{id}/apps/{source}/scan", post(scan_node_app))
        .route("/api/nodes/{id}/release", post(release_node).route_layer(authorize_write.clone()))
        .route("/api/nodes/{id}/force-release", post(force_release_node))
        .route("/api/nodes/{id}/claim", post(claim_node))
        .route("/api/nodes/{id}/roles/init-template", post(init_node_roles).route_layer(authorize_write.clone()))
        .route("/api/nodes/{id}/roles/{role}/check", post(check_node_role).route_layer(authorize_write.clone()))
        .route(
            "/api/tasks",
            post(create_task)
                .route_layer(authorize_write.clone())
                .get(list_tasks),
        )
        .route("/api/tasks/{id}", get(task_detail))
        .route("/api/runs/{id}", get(run_detail))
        .route(
            "/api/runs/{id}/cancel",
            post(cancel_run).route_layer(authorize_write.clone()),
        )
        .route(
            "/api/runs/{id}/reviews",
            post(reviews::submit)
                .route_layer(authorize_write.clone())
                .get(reviews::list),
        )
        .route("/api/reviews/stats", get(reviews::stats))
        .route("/api/reviews/export", get(reviews::export))
        .route(
            "/api/runs/{id}/files/{name}",
            get(files::get).put(files::put),
        )
        .route("/api/runs/{id}/recording-index", put(files::put_recording_index))
        .route("/api/runs/{id}/scripts", put(crate::scripts_api::put))
        .route("/api/runs/{id}/script-source", get(crate::scripts_api::source))
        .route("/api/cases/{id}/scripts", get(crate::scripts_api::list))
        .route("/api/projects/{project}/scripts", get(crate::scripts_api::list_project))
        .route("/api/projects/{project}/cases/{code}/approved-script", get(crate::scripts_api::approved))
        .route("/api/scripts/{id}/regressions", put(crate::scripts_api::regression))
        .route("/api/scripts/{id}/regress", post(crate::scripts_api::request_regression).route_layer(authorize_write.clone()))
        .route("/api/regression-requests/{id}/cancel", post(crate::scripts_api::cancel_request).route_layer(authorize_write.clone()))
        .route("/api/regression-requests/{id}/result", put(crate::scripts_api::finish_request))
        .route("/api/node/regression-requests/claim", put(crate::scripts_api::claim_request))
        .route("/api/node/regression-requests/abandon", put(crate::scripts_api::abandon_requests))
        .route("/api/scripts/{id}/heal-source", get(crate::scripts_api::heal_source))
        .route("/api/scripts/{id}/triage", put(crate::scripts_api::submit_triage))
        .route("/api/scripts/{id}/files", get(crate::scripts_api::files))
        .route("/api/scripts/{id}/self-check", put(crate::scripts_api::self_check))
        .route("/api/scripts/{id}/approve", post(crate::scripts_api::approve).route_layer(authorize_write.clone()))
        .route("/api/scripts/{id}/retire", post(crate::scripts_api::retire).route_layer(authorize_write.clone()))
        .route("/api/scripts/{id}/reject", post(crate::scripts_api::reject).route_layer(authorize_write.clone()))
        .with_state(ApiState {
            app,
            paths,
            evidence_signer: evidence_sign::process_signer(),
        })
}

/// 允许哪些 Host / Origin：回环地址一直允许；`lan` 为真时再加上**私有网段的 IPv4 字面量**
/// （10/8、172.16/12、192.168/16）。**域名一律拒绝**——这样别人的网页即使把域名解析到你的内网地址
/// （DNS 重绑定），请求里的 Host 仍是它的域名，照样被挡住。
fn authority_allowed(value: &str, lan: bool) -> bool {
    let Ok(authority) = value.parse::<Authority>() else {
        return false;
    };
    let host = authority.host();
    // 保留原始主机名做比较，拒绝用户信息、末尾的点和其它可解析成回环地址的写法。
    let local = matches!(host, "127.0.0.1" | "[::1]") || host.eq_ignore_ascii_case("localhost");
    let private_lan = lan && is_private_ipv4_literal(host);
    (local || private_lan)
        && match value.strip_prefix(host) {
            Some("") => true,
            Some(rest) => rest.strip_prefix(':').is_some_and(|port| {
                !port.is_empty()
                    && port.bytes().all(|b| b.is_ascii_digit())
                    && authority.port_u16().is_some()
            }),
            None => false,
        }
}

/// 严格的点分十进制私有 IPv4：不接受 `0x`、八进制、省略段等会被别的解析器当成别的地址的写法。
fn is_private_ipv4_literal(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 || parts.iter().any(|p| p.is_empty() || p.len() > 3 || (p.len() > 1 && p.starts_with('0')) || !p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let Ok(o) = parts.iter().map(|p| p.parse::<u8>()).collect::<Result<Vec<_>, _>>() else {
        return false;
    };
    o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
}

fn allowed_origin(origin: &str, lan: bool) -> bool {
    origin.split_once("://").is_some_and(|(scheme, authority)| {
        (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            && authority_allowed(authority, lan)
    })
}

fn single_header(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(first)
}

pub(crate) async fn browser_write_guard(request: Request, next: Next, lan: bool) -> Response {
    if request.method() == axum::http::Method::POST {
        let headers = request.headers();
        let allowed = single_header(headers, header::HOST).is_some_and(|h| authority_allowed(h, lan))
            && (!headers.contains_key(header::ORIGIN)
                || single_header(headers, header::ORIGIN).is_some_and(|o| allowed_origin(o, lan)))
            && single_header(headers, HeaderName::from_static("x-requested-with"))
                == Some("ai-test-platform")
            && single_header(headers, header::CONTENT_TYPE)
                .is_some_and(|v| v.eq_ignore_ascii_case("application/json"));
        if !allowed {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    next.run(request).await
}

/// 模型配置：`roles` 列表，每项是「某个岗位（执行员 / 判定员）可选的一个来源和它的模型」。
/// 新增角色或模型只改配置文件，不改代码。文件里的格式和节点上报的角色配置是同一个类型。
#[derive(Deserialize)]
struct Models {
    roles: Vec<RoleConfig>,
}

fn find_model<'a>(role: &'a RoleConfig, name: &str) -> Option<&'a protocol::RoleModelOption> {
    role.models.iter().find(|m| m.model == name)
}

/// 目前节点真正能跑的来源组合；还没有运行器的来源不让进配置，免得页面上能选、实际跑不了。
fn source_supported(role: &RoleConfig) -> bool {
    match role.slot {
        RoleSlot::Executor => role.source == ModelSource::CodexExec,
        RoleSlot::Judge => matches!(role.source, ModelSource::ClaudeCli | ModelSource::Agy),
        // 要看 UI 截图：Claude（stream-json 图片输入）和 Codex（-i）都实测能看图；agy 的图片输入还没验证，先不开放
        RoleSlot::CaseWriter => matches!(role.source, ModelSource::ClaudeCli | ModelSource::CodexExec),
        // 管理员只说话（纯文本），三个本机来源都行
        RoleSlot::Planner => matches!(role.source, ModelSource::ClaudeCli | ModelSource::CodexExec | ModelSource::Agy),
    }
}

fn role_valid(role: &RoleConfig) -> bool {
    !role.id.trim().is_empty()
        && !role.name.trim().is_empty()
        && source_supported(role)
        && !role.models.is_empty()
        && role.models.iter().all(|m| !m.model.trim().is_empty() && m.efforts.iter().all(|e| !e.trim().is_empty()))
        && find_model(role, &role.default.model).is_some_and(|m| match &role.default.effort {
            Some(e) => m.efforts.contains(e),
            None => m.efforts.is_empty(),
        })
}

/// 一组角色是否合法：id 不重复、每个角色合法、两个岗位都至少一个、每个岗位不超过上限。
fn roles_valid(roles: &[RoleConfig]) -> bool {
    let unique = roles.iter().enumerate().all(|(i, r)| roles[..i].iter().all(|o| o.id != r.id));
    let within_limit = [RoleSlot::Executor, RoleSlot::Judge, RoleSlot::CaseWriter, RoleSlot::Planner]
        .iter()
        .all(|slot| roles.iter().filter(|r| r.slot == *slot).count() <= RoleSlot::MAX_PER_SLOT);
    unique && within_limit && roles.iter().all(role_valid) && [RoleSlot::Executor, RoleSlot::Judge].iter().all(|slot| roles.iter().any(|r| r.slot == *slot))
}

/// 请求没写角色时用该岗位的第一个角色。
fn pick<'a>(roles: &'a [RoleConfig], slot: RoleSlot, role: Option<&str>) -> Option<&'a RoleConfig> {
    roles.iter().find(|r| r.slot == slot && role.is_none_or(|id| r.id == id))
}

impl Models {
    fn valid(&self) -> bool {
        roles_valid(&self.roles)
    }
}

async fn read_models(path: &FilePath) -> Result<(Vec<u8>, Models), ApiError> {
    let config_error = |message: &str| (StatusCode::INTERNAL_SERVER_ERROR, message.to_owned());
    let bytes = tokio::fs::read(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            config_error("模型配置文件不存在，请检查 PLATFORM_MODELS 或 config/models.json")
        } else {
            config_error("无法读取模型配置文件")
        }
    })?;
    let invalid = || config_error("模型配置文件格式或角色选项无效，请检查模型列表和默认值");
    let models: Models = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if !models.valid() {
        return Err(invalid());
    }
    Ok((bytes, models))
}

async fn models(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    let (bytes, _) = read_models(&state.paths.models).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    ))
}

// ---------------------------------------------------------------- 节点的角色与模型配置（每个节点各自一份）

pub(crate) async fn require_browser_user(state: &ApiState, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    users::browser_user(state.app.pool(), headers)
        .await
        .map_err(|e| internal("校验用户失败", e))?
        .require()
        .map_err(|message| (StatusCode::UNAUTHORIZED, message.into()))
}

/// 保存角色、检测和检查本机应用都需要确认发起人和节点属主。
/// 别人的节点、没有属主的节点一律 403，不存在 404。**只读的查看不走这里**——数据是公开的，谁都能看，只是不能用别人的额度。
async fn require_owned(state: &ApiState, headers: &axum::http::HeaderMap, id: &str) -> Result<Uuid, ApiError> {
    let node: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个节点".to_string()))?;
    let user = require_browser_user(state, headers).await?;
    match crate::users::check_node_owner(state.app.pool(), node, user).await.map_err(|e| internal("校验节点属主失败", e))? {
        crate::users::NodeOwner::Owned => Ok(node),
        crate::users::NodeOwner::Missing => Err((StatusCode::NOT_FOUND, "没有这个节点".to_string())),
        crate::users::NodeOwner::NotYours => Err((StatusCode::FORBIDDEN, "这是别人的节点：你能看，但只能修改和使用自己的节点（用的是各人自己的额度）".to_string())),
    }
}

/// 只读查看用：节点要存在；顺便告诉前端「这个节点是不是你的」，好决定哪些按钮可用。
/// 没填名字时 `mine = null`。名字未登记或需要登录时 `mine = false`，并返回提示。
async fn viewer(state: &ApiState, headers: &axum::http::HeaderMap, id: &str) -> Result<(Uuid, Viewer), ApiError> {
    let node: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个节点".to_string()))?;
    let exists = state.app.node_exists(node).await.map_err(|e| internal("查询节点失败", e))?;
    if !exists {
        return Err((StatusCode::NOT_FOUND, "没有这个节点".to_string()));
    }
    let viewer = match users::browser_user(state.app.pool(), headers).await.map_err(|e| internal("校验用户失败", e))? {
        BrowserUser::Identified(user) => {
            let owned = matches!(crate::users::check_node_owner(state.app.pool(), node, user).await.map_err(|e| internal("校验节点属主失败", e))?, crate::users::NodeOwner::Owned);
            // 节点没有主人：这个名字可以认领它（页面显示「认领这个节点」）
            let unowned: bool = !owned
                && sqlx::query_scalar("select owner_user_id is null from nodes where id = $1").bind(node).fetch_one(state.app.pool()).await.map_err(|e| internal("查询节点属主失败", e))?;
            let name_problem = unowned.then(|| "这个节点还没有主人：点「认领这个节点」就归你，用的是你自己的额度".to_string());
            Viewer { mine: Some(owned), name_problem, unowned }
        }
        BrowserUser::Unspecified => Viewer { mine: None, name_problem: None, unowned: false },
        BrowserUser::Unregistered(name) => Viewer { mine: Some(false), name_problem: Some(format!("「{name}」还没有登记：没有任何节点是用这个名字（NODE_USER_NAME）启动的")), unowned: false },
        BrowserUser::LoginRequired => Viewer { mine: Some(false), name_problem: Some(users::NEED_LOGIN_MESSAGE.into()), unowned: false },
    };
    Ok((node, viewer))
}

#[derive(Serialize)]
struct Viewer {
    mine: Option<bool>,
    name_problem: Option<String>,
    /// 节点还没有主人，当前名字可以认领
    unowned: bool,
}

#[derive(Serialize)]
struct NodeRolesView {
    online: bool,
    roles: Vec<protocol::RoleReport>,
    #[serde(flatten)]
    viewer: Viewer,
}

/// 节点当前的角色摘要。节点离线时 `online=false` 且不返回角色（上一次的报告可能已过期）。Key 永远不在里面。
async fn node_roles(State(state): State<ApiState>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let (node, viewer) = viewer(&state, &headers, &id).await?;
    let online = state.app.node_connected(node).await;
    let roles = if online { state.app.roles_report(node).map(|r| r.roles).unwrap_or_default() } else { vec![] };
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(NodeRolesView { online, roles, viewer })))
}

#[derive(Deserialize)]
struct SetRolesRequest {
    roles: Vec<protocol::RoleSet>,
}

/// 整体替换节点的角色配置：服务端校验后转给节点写入本机。**Key 只在这里经手一次**：不写库、不写日志，
/// `RoleSet` 的 Debug 也隐藏了它。
async fn set_node_roles(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: Result<Json<SetRolesRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let node = require_owned(&state, &headers, &id).await?;
    let Json(request) = body.map_err(|_| bad_request("角色配置格式无效"))?;
    let configs: Vec<RoleConfig> = request.roles.iter().map(|r| r.config.clone()).collect();
    if !roles_valid(&configs) {
        return Err(bad_request("角色配置无效：id 不能重复，执行员和判定员各至少一个，每个岗位最多 3 个，来源要和岗位匹配且已有运行器，默认模型要在模型列表里"));
    }
    push_roles(&state, node, request.roles).await
}

async fn push_roles(state: &ApiState, node: Uuid, roles: Vec<protocol::RoleSet>) -> Result<StatusCode, ApiError> {
    if !state.app.send_to(node, protocol::ServerMsg::SetRoles(protocol::SetRoles { roles })).await {
        return Err((StatusCode::CONFLICT, "节点离线，无法保存角色配置".to_string()));
    }
    Ok(StatusCode::ACCEPTED)
}

/// 用服务端 `models.json` 里的角色给节点做初始配置（不带 Key）。只在节点还没有任何角色时允许，不覆盖已有配置。
async fn init_node_roles(State(state): State<ApiState>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Result<StatusCode, ApiError> {
    let node = require_owned(&state, &headers, &id).await?;
    if state.app.roles_report(node).is_some_and(|r| !r.roles.is_empty()) {
        return Err((StatusCode::CONFLICT, "节点已经有角色配置，不能用模板覆盖".to_string()));
    }
    let (_, models) = read_models(&state.paths.models).await?;
    let roles = models.roles.into_iter().map(|config| protocol::RoleSet { config, api_key: None }).collect();
    push_roles(&state, node, roles).await
}

/// 解除节点绑定：只有这个节点的主人能解除。解除后节点没有主人，任何人可以认领。
/// 注意：节点若仍用原来的 `NODE_USER_NAME` 启动，重新连上来会再绑回去。
async fn release_node(State(state): State<ApiState>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Result<StatusCode, ApiError> {
    let node = require_owned(&state, &headers, &id).await?;
    clear_owner(&state, node).await
}

async fn clear_owner(state: &ApiState, node: Uuid) -> Result<StatusCode, ApiError> {
    sqlx::query("update nodes set owner_user_id = null where id = $1").bind(node).execute(state.app.pool()).await.map_err(|e| internal("解除节点绑定失败", e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Credentials {
    name: String,
    password: String,
}

fn credential_error(message: &str) -> ApiError {
    tracing::error!("{message}");
    (StatusCode::INTERNAL_SERVER_ERROR, message.into())
}

fn password_locked() -> ApiError {
    (StatusCode::TOO_MANY_REQUESTS, "密码尝试次数过多，请等待 5 分钟后再试".into())
}

/// 认领一个没有主人的节点：要先说明你是谁（有密码的名字要登录）。
/// 已经是你的：直接成功；属于别人：409，不能抢；认领和占用在同一条 update 里，两人同时认领只有一个成功。
async fn claim_node(State(state): State<ApiState>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Result<StatusCode, ApiError> {
    let node: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个节点".to_string()))?;
    let user = require_browser_user(&state, &headers).await?;
    let done = sqlx::query("update nodes set owner_user_id = $1 where id = $2 and owner_user_id is null")
        .bind(user)
        .bind(node)
        .execute(state.app.pool())
        .await
        .map_err(|e| internal("认领节点失败", e))?;
    if done.rows_affected() == 1 {
        return Ok(StatusCode::NO_CONTENT);
    }
    match crate::users::check_node_owner(state.app.pool(), node, user).await.map_err(|e| internal("校验节点属主失败", e))? {
        crate::users::NodeOwner::Owned => Ok(StatusCode::NO_CONTENT),
        crate::users::NodeOwner::Missing => Err((StatusCode::NOT_FOUND, "没有这个节点".to_string())),
        crate::users::NodeOwner::NotYours => Err((StatusCode::CONFLICT, "这个节点已经有主人了，不能认领；让主人或管理员先解除绑定".to_string())),
    }
}

/// 管理员强制解除：用管理员账号（名字 + 密码）验证，不要求是节点主人，节点离线也行。
async fn force_release_node(State(state): State<ApiState>, Path(id): Path<String>, body: Result<Json<Credentials>, JsonRejection>) -> Result<StatusCode, ApiError> {
    let Json(body) = body.map_err(|e| (e.status(), "管理员账号或密码格式无效".into()))?;
    let name = body.name.trim();
    let checked = state.app.0.password_auth.check(state.app.pool(), name, body.password, true)
        .await.map_err(|_| credential_error("校验管理员失败"))?;
    match checked {
        PasswordCheck::Matched(_) => {}
        PasswordCheck::Rejected => return Err((StatusCode::FORBIDDEN, "管理员账号或密码不对".into())),
        PasswordCheck::Locked => return Err(password_locked()),
    }
    let node: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个节点".to_string()))?;
    if !state.app.node_exists(node).await.map_err(|e| internal("查询节点失败", e))? {
        return Err((StatusCode::NOT_FOUND, "没有这个节点".to_string()));
    }
    tracing::warn!("管理员 {name} 强制解除节点 {node} 的绑定");
    clear_owner(&state, node).await
}

async fn check_node_role(State(state): State<ApiState>, Path((id, role)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Result<StatusCode, ApiError> {
    let node = require_owned(&state, &headers, &id).await?;
    if !state.app.roles_report(node).is_some_and(|r| r.roles.iter().any(|x| x.config.id == role)) {
        return Err((StatusCode::NOT_FOUND, "节点上没有这个角色".to_string()));
    }
    if !state.app.send_to(node, protocol::ServerMsg::CheckRole { role_id: role }).await {
        return Err((StatusCode::CONFLICT, "节点离线，无法检测".to_string()));
    }
    Ok(StatusCode::ACCEPTED)
}

// ---------------------------------------------------------------- 登记名字和浏览器会话

/// 所有登记过的名字（节点启动带 `NODE_USER_NAME` 时登记），给页面「我是谁」做提示。名字不是机密。
async fn list_users(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    let names = crate::users::list_names(state.app.pool()).await.map_err(|e| internal("查询用户失败", e))?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(names)))
}

#[derive(Deserialize)]
struct NewUser {
    name: String,
    password: String,
    /// 顺便认领的节点（没有主人的才行）；不填就只创建名字
    #[serde(default)]
    claim_node: Option<Uuid>,
}

/// 页面上创建名字：输入名字和密码，名字没被占就登记成功。密码只存哈希，不回显、不进日志。
async fn create_user(State(state): State<ApiState>, body: Result<Json<NewUser>, JsonRejection>) -> Result<impl IntoResponse, ApiError> {
    let Json(body) = body.map_err(|e| (e.status(), "创建名字的请求格式无效".into()))?;
    let Some(name) = crate::users::clean_name(&body.name) else {
        return Err((StatusCode::BAD_REQUEST, "名字要 1 到 40 个字，不能有换行等控制字符".to_string()));
    };
    let chars = body.password.chars().count();
    if !(crate::users::MIN_PASSWORD_CHARS..=crate::users::MAX_PASSWORD_CHARS).contains(&chars) {
        return Err((StatusCode::BAD_REQUEST, format!("密码要 {} 到 {} 个字符", crate::users::MIN_PASSWORD_CHARS, crate::users::MAX_PASSWORD_CHARS)));
    }
    match users::register_with_password(state.app.pool(), &name, &body.password, body.claim_node).await.map_err(|_| credential_error("创建名字失败"))? {
        crate::users::Register::Created => Ok((StatusCode::CREATED, Json(serde_json::json!({ "name": name })))),
        crate::users::Register::Taken => Err((StatusCode::CONFLICT, format!("「{name}」已经有人用了，换一个名字"))),
        crate::users::Register::NodeUnavailable => Err((StatusCode::CONFLICT, "这个节点不存在，或已经有主人了；名字没有创建，请换一个节点或不选节点".to_string())),
    }
}

async fn login(State(state): State<ApiState>, body: Result<Json<Credentials>, JsonRejection>) -> Result<impl IntoResponse, ApiError> {
    let rejected = || (StatusCode::FORBIDDEN, "名字或密码不对".into());
    let Json(body) = body.map_err(|_| rejected())?;
    let name = users::clean_name(&body.name).ok_or_else(rejected)?;
    let checked = state.app.0.password_auth.check(state.app.pool(), &name, body.password, false)
        .await.map_err(|_| credential_error("校验密码失败"))?;
    let user = match checked {
        PasswordCheck::Matched(user) => user,
        PasswordCheck::Rejected => return Err(rejected()),
        PasswordCheck::Locked => return Err(password_locked()),
    };
    let token = users::start_session(state.app.pool(), user).await.map_err(|_| credential_error("创建会话失败"))?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(serde_json::json!({ "name": name, "token": token }))))
}

async fn logout(State(state): State<ApiState>, headers: HeaderMap) -> Result<StatusCode, ApiError> {
    users::end_session(state.app.pool(), &headers).await.map_err(|_| credential_error("注销会话失败"))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- 本机应用环境检查（选应用 → 检查本机 → 取模型）

fn parse_source(text: &str) -> Option<ModelSource> {
    serde_json::from_value(serde_json::Value::String(text.to_string())).ok()
}

#[derive(Serialize)]
struct AppView {
    source: ModelSource,
    /// 检查过没有；没检查过时其余字段是占位
    scanned: bool,
    #[serde(flatten)]
    scan: Option<protocol::AppScan>,
}

#[derive(Serialize)]
struct NodeAppsView {
    online: bool,
    apps: Vec<AppView>,
    #[serde(flatten)]
    viewer: Viewer,
}

/// 四个应用各一项（没检查过的 `scanned = false`）。节点离线时只告诉 `online = false`，不返回上一次的结果（可能已过期）。
async fn node_apps(State(state): State<ApiState>, Path(id): Path<String>, headers: axum::http::HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let (node, viewer) = viewer(&state, &headers, &id).await?;
    let online = state.app.node_connected(node).await;
    let scans = if online { state.app.app_scans(node) } else { Default::default() };
    let apps = [ModelSource::ClaudeCli, ModelSource::CodexExec, ModelSource::Agy, ModelSource::DeepSeek]
        .into_iter()
        .map(|source| match scans.get(&source) {
            Some(scan) => AppView { source, scanned: true, scan: Some(scan.clone()) },
            None => AppView { source, scanned: false, scan: None },
        })
        .collect();
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(NodeAppsView { online, apps, viewer })))
}

/// 让节点检查本机有没有这个应用、怎么登录的、有哪些模型。只跑本地命令，不调模型、不花额度，所以不要审核口令，
/// 但仍要个人令牌且节点是自己的。
async fn scan_node_app(State(state): State<ApiState>, Path((id, source)): Path<(String, String)>, headers: axum::http::HeaderMap) -> Result<StatusCode, ApiError> {
    let node = require_owned(&state, &headers, &id).await?;
    let source = parse_source(&source).ok_or((StatusCode::NOT_FOUND, "没有这个应用".to_string()))?;
    if !state.app.send_to(node, protocol::ServerMsg::ScanApp { source }).await {
        return Err((StatusCode::CONFLICT, "节点离线，无法检查本机环境".to_string()));
    }
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct CreateRequest {
    project: String,
    node_id: Uuid,
    case_codes: Vec<String>,
    executor: ExecutorChoice,
    judge: JudgeChoice,
}

#[derive(Deserialize)]
struct ExecutorChoice {
    role: Option<String>,
    model: String,
    effort: Option<String>,
}

#[derive(Deserialize)]
struct JudgeChoice {
    role: Option<String>,
    model: String,
}

fn bad_request(message: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}

fn run_error(error: RunError, action: &str) -> ApiError {
    let status = match error {
        RunError::Invalid(_) => StatusCode::BAD_REQUEST,
        RunError::Illegal { .. } => StatusCode::CONFLICT,
        RunError::NotFound(_) => StatusCode::NOT_FOUND,
        RunError::Db(e) => return internal(action, e),
    };
    (status, error.to_string())
}

/// 校验发起人和节点属主（规则 9：只能用自己的节点）。节点不存在时交给后面的校验。
pub(crate) async fn owner_check(state: &ApiState, headers: &axum::http::HeaderMap, node: Uuid) -> Result<Uuid, ApiError> {
    let user = require_browser_user(state, headers).await?;
    match crate::users::check_node_owner(state.app.pool(), node, user).await.map_err(|e| internal("校验节点属主失败", e))? {
        crate::users::NodeOwner::NotYours => Err((StatusCode::FORBIDDEN, "只能使用自己的节点发起任务（数据大家都能看、都能跑，但用的是各人自己的额度）".to_string())),
        crate::users::NodeOwner::Owned | crate::users::NodeOwner::Missing => Ok(user),
    }
}

async fn create_task(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
    body: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<TaskCreated>), ApiError> {
    // 任务只能发到发起人自己的节点（规则 9）。
    let Json(request) =
        body.map_err(|_| bad_request("任务参数格式无效，请检查项目、节点、用例、模型和推理强度"))?;
    if crate::users::name_from_headers(&headers).is_none() {
        return Err((StatusCode::UNAUTHORIZED, crate::users::NEED_NAME_MESSAGE.to_string()));
    }
    // 节点上报过角色（新版节点）：只能用该节点自己上报、检测通过的角色，所以要先确认节点是发起人的，
    // 否则别人能借错误提示探测别人节点的配置。没上报过（旧版节点）：沿用服务端的 models.json
    let report = state.app.roles_report(request.node_id);
    let mut owner_checked = false;
    let candidates: Vec<RoleConfig> = match report {
        Some(report) => {
            owner_check(&state, &headers, request.node_id).await?;
            owner_checked = true;
            if report.roles.is_empty() {
                return Err(bad_request("该节点还没有配置角色，请先在「角色与模型」页配置，或用模板初始化"));
            }
            for wanted in [request.executor.role.as_deref(), request.judge.role.as_deref()].into_iter().flatten() {
                if let Some(r) = report.roles.iter().find(|r| r.config.id == wanted) {
                    if r.check.status != protocol::CheckStatus::Ok {
                        return Err(bad_request(format!("角色「{}」还没有检测通过（当前：{:?}），请先在「角色与模型」页检测", r.config.name, r.check.status)));
                    }
                }
            }
            report.roles.into_iter().filter(|r| r.check.status == protocol::CheckStatus::Ok && source_supported(&r.config)).map(|r| r.config).collect()
        }
        None => read_models(&state.paths.models).await?.1.roles,
    };
    let executor_role = pick(&candidates, RoleSlot::Executor, request.executor.role.as_deref())
        .ok_or_else(|| bad_request("执行员角色不在可用列表中（没有配置，或还没有检测通过）"))?;
    let executor = find_model(executor_role, &request.executor.model)
        .ok_or_else(|| bad_request("执行员模型不在配置列表中"))?;
    // 有强度档位的模型必须选其中之一；没有档位的（强度已写进模型名）不能带强度。
    let effort_ok = match &request.executor.effort {
        Some(e) => executor.efforts.contains(e),
        None => executor.efforts.is_empty(),
    };
    if !effort_ok {
        return Err(bad_request("该执行员模型不支持所选推理强度"));
    }
    let judge_role = pick(&candidates, RoleSlot::Judge, request.judge.role.as_deref())
        .ok_or_else(|| bad_request("判定员角色不在可用列表中（没有配置，或还没有检测通过）"))?;
    if find_model(judge_role, &request.judge.model).is_none() {
        return Err(bad_request("判定员模型不在配置列表中"));
    }
    if !owner_checked {
        owner_check(&state, &headers, request.node_id).await?;
    }
    let task = NewTask {
        project: request.project,
        node_id: request.node_id,
        case_codes: request.case_codes,
        executor: RoleModel {
            source: executor_role.source,
            model: Some(request.executor.model),
            effort: request.executor.effort,
        },
        judge: RoleModel {
            source: judge_role.source,
            model: Some(request.judge.model),
            effort: None,
        },
    };
    let created = runs::create_task(state.app.pool(), &task)
        .await
        .map_err(|e| run_error(e, "创建任务失败"))?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn cancel_run(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = id
        .parse()
        .map_err(|_| (StatusCode::NOT_FOUND, "没有这个 run".into()))?;
    state
        .app
        .cancel_run(id)
        .await
        .map_err(|e| run_error(e, "取消 run 失败"))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct TaskQuery {
    limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct TaskInfo {
    task_id: Uuid,
    project: String,
    node_id: Uuid,
    created_at: DateTime<Utc>,
    #[sqlx(json)]
    executor: RoleModel,
    #[sqlx(json)]
    judge: RoleModel,
}

#[derive(Serialize)]
struct VerdictCounts {
    passed: i64,
    failed: i64,
    uncertain: i64,
    blocked: i64,
    cancelled: i64,
    running: i64,
}

impl VerdictCounts {
    fn result_summary(&self) -> String {
        [
            ("通过", self.passed),
            ("失败", self.failed),
            ("待人工确认", self.uncertain),
            ("受阻", self.blocked),
            ("已取消", self.cancelled),
            ("进行中", self.running),
        ]
        .into_iter()
        .filter(|&(_, n)| n > 0)
        .map(|(label, n)| format!("{label} {n}"))
        .collect::<Vec<_>>()
        .join("、")
    }
}

#[derive(Serialize)]
struct TaskSummary {
    #[serde(flatten)]
    task: TaskInfo,
    run_count: i64,
    status_counts: BTreeMap<&'static str, i64>,
    result_summary: String,
    verdict_counts: VerdictCounts,
}

async fn list_tasks(
    State(state): State<ApiState>,
    query: Result<Query<TaskQuery>, QueryRejection>,
) -> Result<Json<Vec<TaskSummary>>, ApiError> {
    let Query(query) = query.map_err(|_| bad_request("limit 必须是非负整数"))?;
    let limit = query.limit.unwrap_or(30);
    if limit < 0 {
        return Err(bad_request("limit 必须是非负整数"));
    }
    let rows = sqlx::query(
        "with recent as (
            select id, project_id, node_id, created_at, executor, judge from tasks order by created_at desc, id desc limit $1
         )
         select t.id as task_id, p.name as project, t.node_id, t.created_at, t.executor, t.judge, counts.*
         from recent t join projects p on p.id = t.project_id
         cross join lateral (
            select count(*) as run_count,
                   count(*) filter (where status = 'queued') as queued,
                   count(*) filter (where status = 'assigned') as assigned,
                   count(*) filter (where status = 'running') as running,
                   count(*) filter (where status = 'judging') as judging,
                   count(*) filter (where status = 'finished') as finished,
                   count(*) filter (where status = 'blocked') as blocked,
                   count(*) filter (where status = 'cancelled') as cancelled,
                   count(*) filter (where status = 'node_lost') as node_lost,
                   count(*) filter (where final_result = 'passed') as passed,
                   count(*) filter (where final_result = 'failed') as failed,
                   count(*) filter (where final_result = 'uncertain') as uncertain
            from runs where task_id = t.id
         ) counts
         order by t.created_at desc, t.id desc",
    )
    .bind(limit)
    .fetch_all(state.app.pool())
    .await
    .map_err(|e| internal("查询任务列表失败", e))?;

    let summaries = rows
        .iter()
        .map(|row| {
            let task = sqlx::FromRow::from_row(row)?;
            let run_count = row.get("run_count");
            let status_counts = RunStatus::ALL
                .into_iter()
                .map(|s| (s.as_str(), row.get(s.as_str())))
                .collect();
            let processing: i64 = RunStatus::ALL
                .into_iter()
                .filter(|s| !s.is_terminal())
                .map(|s| row.get::<i64, _>(s.as_str()))
                .sum();
            let verdict_counts = VerdictCounts {
                passed: row.get("passed"),
                failed: row.get("failed"),
                uncertain: row.get("uncertain"),
                blocked: row.get("blocked"),
                cancelled: row.get("cancelled"),
                running: processing,
            };
            Ok(TaskSummary {
                task,
                run_count,
                status_counts,
                result_summary: verdict_counts.result_summary(),
                verdict_counts,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(|e| internal("查询任务列表失败", e))?;
    Ok(Json(summaries))
}

#[derive(Serialize)]
struct TaskDetail {
    #[serde(flatten)]
    task: TaskInfo,
    runs: Vec<RunView>,
}

async fn task_detail(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<TaskDetail>, ApiError> {
    let not_found = || (StatusCode::NOT_FOUND, "没有这个任务".into());
    let id: Uuid = id.parse().map_err(|_| not_found())?;
    let pool = state.app.pool();
    let task = sqlx::query_as(
        "select t.id as task_id, p.name as project, t.node_id, t.created_at, t.executor, t.judge
         from tasks t join projects p on p.id = t.project_id where t.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| internal("查询任务失败", e))?
    .ok_or_else(not_found)?;
    let runs = runs::task_runs(pool, id)
        .await
        .map_err(|e| internal("查询任务的 run 失败", e))?;
    Ok(Json(TaskDetail { task, runs }))
}

#[derive(Serialize, Deserialize)]
struct CaseSnapshot {
    code: String,
    title: String,
    steps: Vec<String>,
    expected: Vec<CaseExpected>,
    /// 只用来展开文字里的 `{{key}}`，**永远不序列化给页面**（里面可能有账号占位等不该外露的东西）
    #[serde(default, skip_serializing)]
    data: Value,
}

#[derive(Serialize, sqlx::FromRow)]
struct RunRecord {
    #[serde(flatten)]
    #[sqlx(flatten)]
    run: RunView,
    task_id: Uuid,
    #[sqlx(json)]
    case_spec: CaseSnapshot,
    executor_reason: Option<String>,
    cost: Option<Value>,
}

#[derive(Serialize, sqlx::FromRow)]
struct StepView {
    idx: i32,
    at_ms: Option<i64>,
    tool: String,
    args: Value,
    result: Value,
    file_ref: Option<String>,
    ts: DateTime<Utc>,
}

#[derive(Serialize, sqlx::FromRow)]
struct JudgeItemView {
    idx: i32,
    expected_text: String,
    verdict: String,
    evidence: String,
    file_refs: Vec<String>,
}

#[derive(Serialize)]
struct RunDetail {
    #[serde(flatten)]
    record: RunRecord,
    run_steps: Vec<StepView>,
    judge_items: Vec<JudgeItemView>,
    files: Vec<String>,
    file_urls: BTreeMap<String, String>,
    recordings: Vec<SignedRecording>,
    evidence_expires_at: i64,
    evidence_key_id: Uuid,
}

#[derive(Serialize)]
struct SignedRecording {
    #[serde(flatten)]
    recording: files::RecordingView,
    url: String,
}

async fn run_detail(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id: Uuid = id
        .parse()
        .map_err(|_| (StatusCode::NOT_FOUND, "没有这个 run".into()))?;
    let detail = load_run(state.app.pool(), id, state.evidence_signer)
        .await
        .map_err(|e| internal("查询 run 详情失败", e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "没有这个 run".into()))?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(detail)))
}

async fn load_run(
    pool: &sqlx::PgPool,
    id: Uuid,
    signer: &evidence_sign::Signer,
) -> Result<Option<RunDetail>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // 防止本次读取跨过判定完成的事务，状态和逐条预期从同一快照读取。
    sqlx::query("set transaction isolation level repeatable read read only")
        .execute(&mut *tx)
        .await?;
    let record: Option<RunRecord> = sqlx::query_as(
        "select r.id as run_id, r.case_spec->>'code' as code, r.status, r.executor_result, r.judge_status,
                r.final_result, r.blocked_reason, r.case_version, r.queued_at, r.started_at, r.finished_at, d.serial as device_serial,
                r.task_id, r.case_spec, r.executor_reason, r.cost
         from runs r left join devices d on d.id = r.device_id where r.id = $1",
    )
    .bind(id).fetch_optional(&mut *tx).await?;
    let Some(mut record) = record else {
        return Ok(None);
    };
    let mut run_steps: Vec<StepView> = sqlx::query_as(
        "select idx, at_ms, tool, args, result, file_ref, ts from run_steps where run_id = $1 order by idx",
    )
    .bind(id).fetch_all(&mut *tx).await?;
    let mut judge_items: Vec<JudgeItemView> = sqlx::query_as(
        "select idx, expected_text, verdict, evidence, file_refs from judge_items where run_id = $1 order by idx",
    )
    .bind(id).fetch_all(&mut *tx).await?;
    let files = files::names(&mut tx, id).await?;
    let recordings = files::recordings(&mut tx, id).await?;
    tx.commit().await?;

    // 先用用例数据展开 `{{key}}`（页面上才看得懂「找到 tst_swipe_zz」而不是「找到 {{target}}」），再脱敏账号占位
    let data = record.case_spec.data.clone();
    record.executor_reason = record.executor_reason.as_deref().map(public_text);
    record.case_spec.title = public_text(&expand_data(&record.case_spec.title, &data));
    for step in &mut record.case_spec.steps {
        *step = public_text(&expand_data(step, &data));
    }
    for expected in &mut record.case_spec.expected {
        expected.text = public_text(&expand_data(&expected.text, &data));
    }
    for item in &mut judge_items {
        item.expected_text = public_text(&expand_data(&item.expected_text, &data));
    }
    for step in &mut run_steps {
        scrub_fields(&mut step.args);
        scrub_fields(&mut step.result);
        step.file_ref = step
            .file_ref
            .take()
            .filter(|name| files::valid_reference(name));
    }
    if let Some(cost) = &mut record.cost {
        // 用量来自执行器统计，与请求身份无关。
        let usage = cost
            .as_object_mut()
            .and_then(|fields| fields.remove("tokens"));
        scrub_fields(cost);
        if let (Some(mut usage), Some(fields)) = (usage, cost.as_object_mut()) {
            scrub_fields(&mut usage);
            fields.insert("tokens".into(), usage);
        }
    }
    for item in &mut judge_items {
        item.expected_text = public_text(&item.expected_text);
        item.evidence = public_text(&item.evidence);
        item.file_refs.retain(|reference| files::valid_judge_reference(reference));
    }

    let evidence_expires_at = Utc::now().timestamp() + evidence_sign::TTL_SECS;
    let file_urls = files
        .iter()
        .map(|name| (name.clone(), signer.url(id, name, evidence_expires_at)))
        .collect();
    let recordings = recordings
        .into_iter()
        .map(|recording| SignedRecording {
            url: signer.url(id, &recording.name, evidence_expires_at),
            recording,
        })
        .collect();
    Ok(Some(RunDetail {
        record,
        run_steps,
        judge_items,
        files,
        file_urls,
        recordings,
        evidence_expires_at,
        evidence_key_id: signer.id,
    }))
}

/// 把文字里 `{{key}}` 换成用例数据里的值；`{{account.*}}` 和找不到的 key 原样保留（账号由 `public_text` 脱敏）。
/// 与 `agent::prompts::render` 的规则一致。
pub(crate) fn expand_data(text: &str, data: &Value) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = after[..end].trim();
        let hit = (!key.starts_with("account.")).then(|| data.get(key)).flatten();
        match hit {
            Some(Value::String(v)) => out.push_str(v),
            Some(v) => out.push_str(&v.to_string()),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

pub(crate) fn public_text(mut text: &str) -> String {
    let mut out = String::new();
    while let Some((before, placeholder)) = text.split_once("{{account.") {
        out.push_str(before);
        out.push_str("（已脱敏）");
        text = placeholder.split_once("}}").map_or("", |(_, after)| after);
    }
    out.push_str(text);
    out
}

fn scrub_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|key, value| {
                let key = key.to_ascii_lowercase();
                let sensitive = [
                    "password",
                    "passwd",
                    "pwd",
                    "lease",
                    "username",
                    "account",
                    "credential",
                    "token",
                    "secret",
                    "authorization",
                ]
                .iter()
                .any(|part| key.contains(part));
                if !sensitive {
                    scrub_fields(value);
                }
                !sensitive
            });
        }
        Value::Array(items) => items.iter_mut().for_each(scrub_fields),
        Value::String(text) => *text = public_text(text),
        _ => {}
    }
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;

#[cfg(test)]
mod evidence_tests {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::ServiceExt;
    use axum::body::{Body, to_bytes};
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    async fn get(
        mut service: impl ServiceExt<Request, Response = Response, Error = Infallible>,
        uri: &str,
    ) -> Response {
        let req = axum::http::Request::builder()
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        service.call(req).await.unwrap()
    }

    #[tokio::test]
    async fn 证据读取统一拒绝无效签名() {
        // 使用关闭的连接池，确保签名校验在访问数据库之前完成。
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://127.0.0.1/unused_evidence_db")
            .unwrap();
        pool.close().await;
        let app = routes(
            AppState(Arc::new(crate::Inner {
                cfg: crate::Config {
                    node_token: "synthetic-node-token".into(),
                    hello_timeout: Duration::from_secs(1),
                    heartbeat_s: 1,
                    lost_grace: Duration::from_secs(1),
                },
                pool,
                accounts: None,
                review_auth: reviews::ReviewAuth::new(None),
                password_auth: Default::default(),
                conns: Default::default(),
                roles: Default::default(),
                apps: Default::default(),
                admin_asks: Default::default(),
            })),
            Paths {
                models: PathBuf::new(),
                data: PathBuf::new(),
            },
        );
        let run_id = Uuid::from_u128(1);
        let signer = evidence_sign::process_signer();
        let now = Utc::now().timestamp();
        let exp = now + evidence_sign::TTL_SECS;
        let bare = format!("/api/runs/{run_id}/files/01_after.png");
        let signed = signer.url(run_id, "01_after.png", exp);
        for path in [
            bare.clone(),
            bare.replace("01_after.png", "shot..png"),
            format!("{bare}?exp={exp}"),
            format!("{bare}?exp=invalid&sig=00"),
            format!("{signed}&exp={exp}"),
            signer.url(run_id, "01_after.png", now - 1),
            signed.replace("01_after.png", "02_after.png"),
        ] {
            let response = get(app.clone().into_service(), &path).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(to_bytes(response.into_body(), 0).await.unwrap().is_empty());
        }

        let invalid_name = signer.url(run_id, "shot..png", exp);
        let response = get(app.into_service(), &invalid_name).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
mod expand_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 展开用例数据但不展开账号占位() {
        let d = json!({"target": "tst_swipe_zz", "n": 3, "account": {"username": "x"}});
        assert_eq!(expand_data("找到 {{target}} 后选中 {{n}} 首", &d), "找到 tst_swipe_zz 后选中 3 首");
        assert_eq!(expand_data("用 {{account.username}} 登录", &d), "用 {{account.username}} 登录", "账号占位原样保留，交给脱敏");
        assert_eq!(public_text(&expand_data("用 {{account.username}} 登录", &d)), "用 （已脱敏） 登录");
        assert_eq!(expand_data("{{不存在}} 与 {{target", &d), "{{不存在}} 与 {{target", "找不到的 key 和没闭合的占位都原样保留");
    }

    #[test]
    fn 用例数据不会被序列化给页面() {
        let snap: CaseSnapshot = serde_json::from_value(json!({"code":"T","title":"t","steps":[],"expected":[],"data":{"target":"x","password":"P@ss"}})).unwrap();
        let out = serde_json::to_string(&snap).unwrap();
        assert!(!out.contains("P@ss") && !out.contains("\"data\""), "data 字段绝不能出现在对外的 JSON 里：{out}");
    }
}

