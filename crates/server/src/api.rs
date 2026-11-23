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
use protocol::{CaseExpected, ModelSource, RoleModel};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use crate::runs::{self, NewTask, RunError, RunStatus, RunView, TaskCreated};
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

#[derive(Deserialize)]
struct Models {
    executor: ModelGroup,
    judge: ModelGroup,
}

#[derive(Deserialize)]
struct ModelGroup {
    source: ModelSource,
    default: ModelDefault,
    models: Vec<ModelOption>,
}

#[derive(Deserialize)]
struct ModelDefault {
    model: String,
    effort: Option<String>,
}

#[derive(Deserialize)]
struct ModelOption {
    model: String,
    efforts: Vec<String>,
}

impl ModelGroup {
    fn find(&self, name: &str) -> Option<&ModelOption> {
        self.models.iter().find(|m| m.model == name)
    }
}

impl Models {
    fn valid(&self) -> bool {
        self.executor.source == ModelSource::CodexExec
            && self.judge.source == ModelSource::ClaudeCli
            && self.executor.models.iter().all(|m| {
                !m.model.trim().is_empty()
                    && !m.efforts.is_empty()
                    && m.efforts.iter().all(|e| !e.trim().is_empty())
            })
            && self.judge.models.iter().all(|m| !m.model.trim().is_empty())
            && self
                .executor
                .find(&self.executor.default.model)
                .is_some_and(|m| {
                    self.executor
                        .default
                        .effort
                        .as_ref()
                        .is_some_and(|e| m.efforts.contains(e))
                })
            && self.judge.find(&self.judge.default.model).is_some()
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
    model: String,
    effort: String,
}

#[derive(Deserialize)]
struct JudgeChoice {
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

async fn create_task(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
    body: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<TaskCreated>), ApiError> {
    // 发起人身份：个人令牌。任务只能发到发起人自己的节点（规则 9）
    let token = headers.get("x-user-token").and_then(|h| h.to_str().ok()).unwrap_or_default();
    let Json(request) =
        body.map_err(|_| bad_request("任务参数格式无效，请检查项目、节点、用例、模型和推理强度"))?;
    if token.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "需要有效的个人令牌才能发起任务".to_string()));
    }
    let (_, models) = read_models(&state.paths.models).await?;
    let executor = models
        .executor
        .find(&request.executor.model)
        .ok_or_else(|| bad_request("执行员模型不在配置列表中"))?;
    if !executor.efforts.contains(&request.executor.effort) {
        return Err(bad_request("该执行员模型不支持所选推理强度"));
    }
    if models.judge.find(&request.judge.model).is_none() {
        return Err(bad_request("判定员模型不在配置列表中"));
    }
    let user = crate::users::user_by_token(state.app.pool(), token)
        .await
        .map_err(|e| internal("校验发起人失败", e))?
        .ok_or((StatusCode::UNAUTHORIZED, "需要有效的个人令牌才能发起任务".to_string()))?;
    match crate::users::check_node_owner(state.app.pool(), request.node_id, user).await.map_err(|e| internal("校验节点属主失败", e))? {
        crate::users::NodeOwner::NotYours => return Err((StatusCode::FORBIDDEN, "只能使用自己的节点发起任务".to_string())),
        crate::users::NodeOwner::Owned | crate::users::NodeOwner::Missing => {}
    }
    let task = NewTask {
        project: request.project,
        node_id: request.node_id,
        case_codes: request.case_codes,
        executor: RoleModel {
            source: ModelSource::CodexExec,
            model: Some(request.executor.model),
            effort: Some(request.executor.effort),
        },
        judge: RoleModel {
            source: ModelSource::ClaudeCli,
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
                conns: Default::default(),
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

