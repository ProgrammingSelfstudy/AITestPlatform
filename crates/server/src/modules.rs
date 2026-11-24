//! 模块、项目级需求文档、用例生成请求（创建任务的必填项都在这里校验）。
//!
//! 规则：
//! - 需求文档、UI 图链接、UI 区域截图**都是必填**；缺哪个就拒绝并说明缺什么。
//! - UI 链接只做记录和跳转：平台和节点都不会去打开它（不用用户在设计工具里的登录态，也不会被诱导访问内网地址）。
//!   AI 只看用户上传的截图。
//! - 文档里出现明文密码就拒绝（规则 2：账号密码只能用 `{{account.xxx}}` 占位符），错误里不回显值。
//! - 生成请求只能发到发起人自己的节点（规则 9）；同一节点上同一模块同时只能有一个未结束的请求。

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::api::ApiState;
use crate::internal;
use crate::users::{self, BrowserUser};

pub const MAX_DOC_CHARS: usize = 200_000;
pub const MAX_LINKS: usize = 5;
pub const MAX_LINK_CHARS: usize = 2000;
pub const MAX_SHOTS: usize = 100;
pub const MAX_SHOT_BYTES: usize = 3 * 1024 * 1024;
/// 全部截图合计的上限（解码后）：100 张都到 3MB 会有 300MB，一个请求扛不住，合计限制在 100MB
pub const MAX_TOTAL_SHOT_BYTES: usize = 100 * 1024 * 1024;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_MAGIC: &[u8] = b"\xff\xd8\xff";

#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub(crate) mod assets;

type Res<T> = Result<T, ModuleError>;

fn invalid<T>(message: impl Into<String>) -> Res<T> {
    Err(ModuleError::Invalid(message.into()))
}

// ---------------------------------------------------------------- 校验

pub fn check_module_name(name: &str) -> Res<String> {
    let name = name.trim();
    let count = name.chars().count();
    if count == 0 || count > 80 {
        return invalid("模块名不能为空，且不超过 80 个字");
    }
    if name.chars().any(char::is_control) {
        return invalid("模块名里不能有控制字符");
    }
    Ok(name.to_string())
}

/// 文档里的明文密码：`密码/password/passwd/pwd` 后面跟冒号或等号再跟一个不是占位符的值。
/// 返回第一处的行号（不含值本身）。
fn plain_password_line(doc: &str) -> Option<usize> {
    let keys = ["密码", "password", "passwd", "pwd"];
    doc.lines().enumerate().find_map(|(i, line)| {
        let lower = line.to_lowercase();
        keys.iter().find_map(|key| {
            let at = lower.find(key)?;
            let rest = line.get(at + key.len()..)?.trim_start();
            let rest = rest.strip_prefix([':', '：', '=']).map(str::trim_start)?;
            let value = rest.split_whitespace().next().unwrap_or("");
            let value = value.trim_matches(|c| matches!(c, '`' | '"' | '\'' | '*' | '，' | ',' | '。'));
            (!value.is_empty() && !value.starts_with("{{account.")).then_some(i + 1)
        })
    })
}

/// 规范化：换行统一成 `\n`、每行去掉行尾空白、去掉首尾空行。**内容相同就是同一份文档**——
/// 只是换行符不同（Windows 的 `\r\n`）、行尾多了空格的，不能被当成两份。
pub fn normalize_doc(raw: &str) -> String {
    raw.replace("\r\n", "\n").replace('\r', "\n").lines().map(str::trim_end).collect::<Vec<_>>().join("\n").trim().to_string()
}

pub fn check_doc(raw: &str) -> Res<String> {
    let doc = normalize_doc(raw);
    let doc = doc.as_str();
    if doc.is_empty() {
        return invalid("需求文档必填：请粘贴或上传需求文档（Markdown 或文本）");
    }
    if doc.chars().count() > MAX_DOC_CHARS {
        return invalid(format!("需求文档太长（最多 {MAX_DOC_CHARS} 个字），请拆分后再提交"));
    }
    if let Some(line) = plain_password_line(doc) {
        return invalid(format!("需求文档第 {line} 行疑似有明文密码（值未显示）。账号密码只能写成 {{{{account.xxx}}}} 占位符，请改完再提交"));
    }
    Ok(doc.to_string())
}

/// 链接只允许 http / https，不带用户名密码，不含空白。
pub fn check_link(link: &str) -> Res<String> {
    let link = link.trim();
    if link.is_empty() || link.chars().count() > MAX_LINK_CHARS || link.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return invalid("UI 图链接格式不对：不能为空、不能含空格，且不超过 2000 个字符");
    }
    let rest = link.strip_prefix("https://").or_else(|| link.strip_prefix("http://"));
    let Some(rest) = rest else { return invalid("UI 图链接必须以 http:// 或 https:// 开头") };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return invalid("UI 图链接不能没有主机名，也不能带用户名密码");
    }
    Ok(link.to_string())
}

pub struct Shot {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

fn decode_shot(index: usize, shot: &ShotInput) -> Res<Shot> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(shot.data_b64.trim())
        .map_err(|_| ModuleError::Invalid(format!("第 {} 张截图不是有效的 base64 数据", index + 1)))?;
    if bytes.len() > MAX_SHOT_BYTES {
        return invalid(format!("第 {} 张截图超过 {} MB，请裁剪到要看的区域再上传", index + 1, MAX_SHOT_BYTES / 1024 / 1024));
    }
    // 以文件头为准，不信上传者声明的类型
    let content_type = if bytes.starts_with(PNG_MAGIC) {
        "image/png"
    } else if bytes.starts_with(JPEG_MAGIC) {
        "image/jpeg"
    } else {
        return invalid(format!("第 {} 张截图不是 PNG 或 JPEG 图片", index + 1));
    };
    Ok(Shot { content_type, bytes })
}

// ---------------------------------------------------------------- 请求体

#[derive(Deserialize)]
pub struct ShotInput {
    pub data_b64: String,
}

#[derive(Deserialize)]
pub struct CasegenInput {
    pub module: String,
    pub doc: String,
    pub ui_links: Vec<String>,
    pub screenshots: Vec<ShotInput>,
    pub node_id: Uuid,
    #[serde(default)]
    pub writer_role: Option<String>,
    #[serde(default)]
    pub reviewer_role: Option<String>,
}

pub struct Validated {
    pub module: String,
    pub doc: String,
    pub links: Vec<String>,
    pub shots: Vec<Shot>,
}

/// 必填项和格式校验（不碰数据库，所以不会因为缺项而先去查库）。
pub fn validate(input: &CasegenInput) -> Res<Validated> {
    let module = check_module_name(&input.module).map_err(|_| ModuleError::Invalid("模块必填：请选择或新建一个模块（名称不超过 80 个字）".into()))?;
    let doc = check_doc(&input.doc)?;
    if input.ui_links.is_empty() {
        return invalid("UI 图链接必填：请至少填一个设计图链接");
    }
    if input.ui_links.len() > MAX_LINKS {
        return invalid(format!("UI 图链接最多 {MAX_LINKS} 个"));
    }
    let links = input.ui_links.iter().map(|l| check_link(l)).collect::<Res<Vec<_>>>()?;
    if input.screenshots.is_empty() {
        return invalid("UI 区域截图必填：请上传至少一张你要我看的那块区域的截图（AI 只看你上传的截图，不会打开链接）");
    }
    if input.screenshots.len() > MAX_SHOTS {
        return invalid(format!("UI 区域截图最多 {MAX_SHOTS} 张"));
    }
    let shots = input.screenshots.iter().enumerate().map(|(i, s)| decode_shot(i, s)).collect::<Res<Vec<_>>>()?;
    let total: usize = shots.iter().map(|s| s.bytes.len()).sum();
    if total > MAX_TOTAL_SHOT_BYTES {
        return invalid(format!("截图合计 {} MB，超过 {} MB 的上限，请少传几张或裁小一点", total.div_ceil(1024 * 1024), MAX_TOTAL_SHOT_BYTES / 1024 / 1024));
    }
    Ok(Validated { module, doc, links, shots })
}

// ---------------------------------------------------------------- 数据库

#[derive(Debug, Serialize)]
pub struct RequestView {
    pub id: Uuid,
    pub status: String,
    pub detail: String,
    pub requested_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ModuleView {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub case_count: i64,
    pub approved_count: i64,
    pub needs_human_count: i64,
    /// 排队、分配、执行、判定中的 run 数。大于 0 就是「测试中」：页面此时才切到指挥台布局
    pub active_runs: i64,
    pub latest_request: Option<RequestView>,
}

async fn project_id(pool: &PgPool, project: &str) -> Res<Uuid> {
    sqlx::query_scalar("select id from projects where name = $1")
        .bind(project)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| ModuleError::NotFound(format!("没有这个项目：{project}")))
}

pub async fn list(pool: &PgPool, project: &str) -> Res<Vec<ModuleView>> {
    let project_id = project_id(pool, project).await?;
    let rows = sqlx::query(
        "select m.id, m.name, m.created_at,
                (select count(*) from test_cases c where c.module_id = m.id) as case_count,
                (select count(*) from test_cases c where c.module_id = m.id and c.review_status = 'approved') as approved_count,
                (select count(*) from test_cases c where c.module_id = m.id and c.review_status = 'needs_human') as needs_human_count,
                (select count(*) from runs x join test_cases c on c.id = x.case_id
                  where c.module_id = m.id and x.status in ('queued','assigned','running','judging')) as active_runs,
                r.id as req_id, r.status as req_status, r.detail as req_detail, r.requested_at as req_at
         from modules m
         left join lateral (select * from casegen_requests q where q.module_id = m.id order by q.requested_at desc limit 1) r on true
         where m.project_id = $1 order by m.created_at, m.name",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| ModuleView {
            id: row.get("id"),
            name: row.get("name"),
            created_at: row.get("created_at"),
            case_count: row.get("case_count"),
            approved_count: row.get("approved_count"),
            needs_human_count: row.get("needs_human_count"),
            active_runs: row.get("active_runs"),
            latest_request: row.get::<Option<Uuid>, _>("req_id").map(|id| RequestView {
                id,
                status: row.get("req_status"),
                detail: row.get("req_detail"),
                requested_at: row.get("req_at"),
            }),
        })
        .collect())
}

/// 取或建模块（同名不重复建）。
async fn upsert_module(conn: &mut sqlx::PgConnection, project_id: Uuid, name: &str) -> Res<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        "insert into modules (project_id, name) values ($1, $2)
         on conflict (project_id, name) do update set name = excluded.name returning id",
    )
    .bind(project_id)
    .bind(name)
    .fetch_one(conn)
    .await?;
    Ok(id)
}

pub async fn create_module(pool: &PgPool, project: &str, name: &str) -> Res<Uuid> {
    let name = check_module_name(name)?;
    let project_id = project_id(pool, project).await?;
    let mut conn = pool.acquire().await?;
    upsert_module(&mut conn, project_id, &name).await
}

pub struct Created {
    pub request_id: Uuid,
    pub module_id: Uuid,
    pub doc_id: Uuid,
}

/// 建生成请求：文档按内容去重存成项目级版本，截图存库，全部在一个事务里。
pub async fn create_request(pool: &PgPool, project: &str, user: Uuid, input: &CasegenInput, v: &Validated) -> Res<Created> {
    let project_id = project_id(pool, project).await?;
    let mut tx = pool.begin().await?;
    let module_id = upsert_module(&mut tx, project_id, &v.module).await?;
    let sha = hex::encode(Sha256::digest(v.doc.as_bytes()));
    let doc_id: Uuid = sqlx::query_scalar(
        "insert into project_docs (project_id, sha256, content) values ($1, $2, $3)
         on conflict (project_id, sha256) do update set sha256 = excluded.sha256 returning id",
    )
    .bind(project_id)
    .bind(&sha)
    .bind(&v.doc)
    .fetch_one(&mut *tx)
    .await?;
    let inserted: Result<Uuid, sqlx::Error> = sqlx::query_scalar(
        "insert into casegen_requests (project_id, module_id, doc_id, node_id, requested_by, ui_links, writer_role, reviewer_role)
         values ($1,$2,$3,$4,$5,$6,$7,$8) returning id",
    )
    .bind(project_id)
    .bind(module_id)
    .bind(doc_id)
    .bind(input.node_id)
    .bind(user)
    .bind(serde_json::json!(v.links))
    .bind(&input.writer_role)
    .bind(&input.reviewer_role)
    .fetch_one(&mut *tx)
    .await;
    let request_id = match inserted {
        Ok(id) => id,
        Err(sqlx::Error::Database(e)) if e.constraint() == Some("casegen_requests_active") => {
            return Err(ModuleError::Conflict("这个模块在该节点上已经有一个排队或运行中的生成请求，请等它结束或先取消".into()));
        }
        Err(e) => return Err(e.into()),
    };
    for (seq, shot) in v.shots.iter().enumerate() {
        sqlx::query("insert into casegen_screenshots (request_id, seq, content_type, bytes, sha256) values ($1,$2,$3,$4,$5)")
            .bind(request_id)
            .bind(seq as i32)
            .bind(shot.content_type)
            .bind(&shot.bytes)
            .bind(hex::encode(Sha256::digest(&shot.bytes)))
            .execute(&mut *tx)
            .await?;
        // 同时记进模块的图库（模块详情页能看、能删）；同一张图已经在里面就不重复存
        sqlx::query(
            "insert into module_screenshots (module_id, name, content_type, bytes, sha256, created_by)
             values ($1,$2,$3,$4,$5,$6) on conflict (module_id, sha256) do nothing",
        )
        .bind(module_id)
        .bind(format!("生成用例时上传的截图 {}", seq + 1))
        .bind(shot.content_type)
        .bind(&shot.bytes)
        .bind(hex::encode(Sha256::digest(&shot.bytes)))
        .bind(user)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Created { request_id, module_id, doc_id })
}

#[derive(Debug, Serialize)]
pub struct RequestDetail {
    pub id: Uuid,
    pub module: String,
    pub status: String,
    pub detail: String,
    pub ui_links: Vec<String>,
    pub screenshot_count: i64,
    pub doc_sha256: String,
    pub requested_at: DateTime<Utc>,
}

pub async fn request_detail(pool: &PgPool, id: Uuid) -> Res<RequestDetail> {
    let row = sqlx::query(
        "select r.id, m.name as module, r.status, r.detail, r.ui_links, d.sha256, r.requested_at,
                (select count(*) from casegen_screenshots s where s.request_id = r.id) as shots
         from casegen_requests r join modules m on m.id = r.module_id join project_docs d on d.id = r.doc_id where r.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ModuleError::NotFound("没有这个生成请求".into()))?;
    let links: serde_json::Value = row.get("ui_links");
    Ok(RequestDetail {
        id: row.get("id"),
        module: row.get("module"),
        status: row.get("status"),
        detail: row.get("detail"),
        ui_links: links.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
        screenshot_count: row.get("shots"),
        doc_sha256: row.get("sha256"),
        requested_at: row.get("requested_at"),
    })
}

// ---------------------------------------------------------------- HTTP

type ApiError = (StatusCode, String);

fn api_error(e: ModuleError, action: &str) -> ApiError {
    match e {
        ModuleError::Invalid(m) => (StatusCode::BAD_REQUEST, m),
        ModuleError::NotFound(m) => (StatusCode::NOT_FOUND, m),
        ModuleError::Conflict(m) => (StatusCode::CONFLICT, m),
        ModuleError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
        ModuleError::Db(e) => internal(action, e),
    }
}

pub(crate) async fn api_list(State(state): State<ApiState>, Path(project): Path<String>) -> Result<Json<Vec<ModuleView>>, ApiError> {
    list(state.app.pool(), &project).await.map(Json).map_err(|e| api_error(e, "查询模块失败"))
}

#[derive(Deserialize)]
pub(crate) struct NewModule {
    name: String,
}

pub(crate) async fn api_create_module(
    State(state): State<ApiState>,
    Path(project): Path<String>,
    body: Result<Json<NewModule>, JsonRejection>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let Json(body) = body.map_err(|_| (StatusCode::BAD_REQUEST, "模块参数格式无效".to_string()))?;
    let id = create_module(state.app.pool(), &project, &body.name).await.map_err(|e| api_error(e, "创建模块失败"))?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "module_id": id }))))
}

/// `POST /api/projects/{project}/casegen-requests`：创建任务的入口（必填项齐了才会建）。
pub(crate) async fn api_create_request(
    State(state): State<ApiState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    body: Result<Json<CasegenInput>, JsonRejection>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let Json(mut input) = body.map_err(|_| (StatusCode::BAD_REQUEST, "请求格式无效，或内容太大（截图合计不要超过约 18MB）".to_string()))?;
    // 先校验必填和格式，再校验身份和节点属主：缺项的提示不依赖库
    let validated = validate(&input).map_err(|e| api_error(e, "校验失败"))?;
    let user = crate::api::owner_check(&state, &headers, input.node_id).await?;
    if !state.app.node_exists(input.node_id).await.map_err(|e| internal("查询节点失败", e))? {
        return Err((StatusCode::NOT_FOUND, "没有这个节点".to_string()));
    }
    // 节点上报过角色（新版节点）：必须有检测通过的「测试用例」角色。请求没指定就用第一个检测通过的；
    // 指定了就必须是它，不退回别的。没上报过（旧版节点）不拦，节点领取时会如实报没有角色
    if let Some(report) = state.app.roles_report(input.node_id) {
        let writers: Vec<_> = report.roles.iter().filter(|r| r.config.slot == protocol::RoleSlot::CaseWriter).collect();
        if writers.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "该节点还没有配置「测试用例」角色，请先在「角色与模型」页配置并检测".to_string()));
        }
        let chosen = match input.writer_role.as_deref() {
            Some(id) => writers.iter().find(|r| r.config.id == id).ok_or((StatusCode::BAD_REQUEST, "所选的测试用例角色不在该节点的配置里".to_string()))?,
            None => writers
                .iter()
                .find(|r| r.check.status == protocol::CheckStatus::Ok)
                .ok_or((StatusCode::BAD_REQUEST, "该节点的测试用例角色都还没有检测通过，请先在「角色与模型」页检测".to_string()))?,
        };
        if chosen.check.status != protocol::CheckStatus::Ok {
            return Err((StatusCode::BAD_REQUEST, format!("测试用例角色「{}」还没有检测通过（当前：{:?}），请先在「角色与模型」页检测", chosen.config.name, chosen.check.status)));
        }
        input.writer_role = Some(chosen.config.id.clone());
    }
    let created = create_request(state.app.pool(), &project, user, &input, &validated).await.map_err(|e| api_error(e, "创建生成请求失败"))?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "request_id": created.request_id, "module_id": created.module_id, "doc_id": created.doc_id }))))
}

pub(crate) async fn api_request_detail(State(state): State<ApiState>, Path(id): Path<String>) -> Result<Json<RequestDetail>, ApiError> {
    let id: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个生成请求".to_string()))?;
    request_detail(state.app.pool(), id).await.map(Json).map_err(|e| api_error(e, "查询生成请求失败"))
}

// ---------------------------------------------------------------- 节点侧：领取、回报生成结果

/// 领取后超过这么久还没回报，当作节点没有回报。
pub const CLAIM_TIMEOUT_MINUTES: i32 = 30;
pub const MAX_CASES: usize = 40;

#[derive(Debug, Serialize)]
pub struct ClaimedShot {
    pub content_type: String,
    pub data_b64: String,
}

/// 节点领到的生成任务。文档和截图随请求一起给，节点不需要再来取。
#[derive(Debug, Serialize)]
pub struct Claimed {
    pub id: Uuid,
    pub project: String,
    pub module: String,
    pub doc: String,
    pub ui_links: Vec<String>,
    pub screenshots: Vec<ClaimedShot>,
    pub writer_role: Option<String>,
    pub reviewer_role: Option<String>,
}

pub async fn claim(pool: &PgPool, node_id: Uuid) -> Res<Option<Claimed>> {
    let mut tx = pool.begin().await?;
    // 领走太久没回报的请求转失败，免得一直占着「同模块只能有一个未结束请求」
    sqlx::query(
        "update casegen_requests set status = 'failed', detail = '节点没有回报生成结果（超时）', finished_at = now()
         where status = 'running' and claimed_at < now() - make_interval(mins => $1)",
    )
    .bind(CLAIM_TIMEOUT_MINUTES)
    .execute(&mut *tx)
    .await?;
    let Some(row) = sqlx::query(
        "select r.id, r.ui_links, r.writer_role, r.reviewer_role, m.name as module, p.name as project, d.content
         from casegen_requests r
         join modules m on m.id = r.module_id join projects p on p.id = r.project_id join project_docs d on d.id = r.doc_id
         where r.node_id = $1 and r.status = 'queued' order by r.requested_at limit 1 for update of r skip locked",
    )
    .bind(node_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.commit().await?;
        return Ok(None);
    };
    let id: Uuid = row.get("id");
    sqlx::query("update casegen_requests set status = 'running', claimed_at = now() where id = $1").bind(id).execute(&mut *tx).await?;
    let shots = sqlx::query("select content_type, bytes from casegen_screenshots where request_id = $1 order by seq")
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let links: serde_json::Value = row.get("ui_links");
    Ok(Some(Claimed {
        id,
        project: row.get("project"),
        module: row.get("module"),
        doc: row.get("content"),
        ui_links: links.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
        screenshots: shots
            .iter()
            .map(|s| ClaimedShot {
                content_type: s.get("content_type"),
                data_b64: base64::engine::general_purpose::STANDARD.encode(s.get::<Vec<u8>, _>("bytes")),
            })
            .collect(),
        writer_role: row.get("writer_role"),
        reviewer_role: row.get("reviewer_role"),
    }))
}

/// 去掉模型可能加的代码围栏，按单独一行的 `---` 切成多个 YAML 文档。
pub fn split_case_docs(text: &str) -> Vec<String> {
    let mut docs = vec![String::new()];
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            continue;
        }
        if trimmed == "---" {
            docs.push(String::new());
            continue;
        }
        let last = docs.last_mut().expect("至少有一项");
        last.push_str(trimmed);
        last.push('\n');
    }
    docs.retain(|d| !d.trim().is_empty());
    docs
}

/// 解析并校验生成的用例：和导入用例同一套规则（`parse_case`），再加几条对「模型写的」更严的：
/// 编号由平台分配（不信模型给的，免得覆盖已有用例）、不能是「故意失败」级别、文字里不能有明文密码。
/// **任何一条不合格整批拒收**，错误里不回显值。
pub fn parse_generated(request_id: Uuid, text: &str) -> Result<Vec<crate::cases::NewCase>, String> {
    let docs = split_case_docs(text);
    if docs.is_empty() {
        return Err("模型没有输出任何用例".into());
    }
    if docs.len() > MAX_CASES {
        return Err(format!("模型输出了 {} 条用例，超过上限 {MAX_CASES}，整批拒收", docs.len()));
    }
    let short = &request_id.simple().to_string()[..6];
    let mut cases = vec![];
    let mut errors = vec![];
    for (i, doc) in docs.iter().enumerate() {
        let source = format!("第 {} 条用例", i + 1);
        match crate::cases::parse_case(doc, &source) {
            Err(e) => errors.push(e),
            Ok(mut case) => {
                if case.should_fail {
                    errors.push(format!("{source}：生成的用例不能是「{}」级别", crate::cases::NEGATIVE_LEVEL));
                    continue;
                }
                let joined = format!("{}\n{}\n{}\n{}\n{}", case.title, case.preconditions.join("\n"), case.steps.join("\n"), case.expected.iter().map(|e| e.text.as_str()).collect::<Vec<_>>().join("\n"), case.note);
                if plain_password_line(&joined).is_some() {
                    errors.push(format!("{source}：文字里疑似有明文密码（值未显示），账号密码只能用 {{{{account.xxx}}}} 占位符"));
                    continue;
                }
                case.code = format!("GEN-{short}-{:02}", i + 1);
                cases.push(case);
            }
        }
    }
    if errors.is_empty() { Ok(cases) } else { Err(errors.join("；")) }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinishInput {
    /// 节点是否拿到了模型的输出
    pub ok: bool,
    /// 失败原因，或成功时的说明（已脱敏）
    pub detail: String,
    /// 模型输出的用例文本（YAML，多条用 `---` 分隔）
    #[serde(default)]
    pub cases_text: Option<String>,
}

/// 回报成功后给节点的用例摘要，节点据此让评审员复核。
#[derive(Debug, Serialize)]
pub struct CaseBrief {
    pub code: String,
    pub title: String,
    pub preconditions: Vec<String>,
    pub steps: Vec<String>,
    pub expected: Vec<String>,
}

pub struct Finished {
    pub status: &'static str,
    pub detail: String,
    pub codes: Vec<String>,
    pub cases: Vec<CaseBrief>,
}

/// 回报结果：只有领走它的节点、且仍在运行中才能回报。成功时用例以 `draft` 入库（不能执行，要先复核和人工确认）。
pub async fn finish(pool: &PgPool, node_id: Uuid, id: Uuid, input: &FinishInput) -> Res<Finished> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "select r.node_id, r.status, r.project_id, r.module_id from casegen_requests r where r.id = $1 for update",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ModuleError::NotFound("没有这个生成请求".into()))?;
    if row.get::<Uuid, _>("node_id") != node_id {
        return Err(ModuleError::Conflict("这个生成请求不是你的节点领走的".into()));
    }
    if row.get::<String, _>("status") != "running" {
        return Err(ModuleError::Conflict("这个生成请求已经不在运行中".into()));
    }
    let detail: String = input.detail.chars().take(1800).collect();
    let (status, detail, codes, briefs) = if !input.ok {
        ("failed", detail, vec![], vec![])
    } else {
        match parse_generated(id, input.cases_text.as_deref().unwrap_or("")) {
            Err(why) => ("failed", format!("生成的用例没有通过校验，整批没有入库：{}", why.chars().take(1700).collect::<String>()), vec![], vec![]),
            Ok(cases) => {
                let project_id: Uuid = row.get("project_id");
                let module_id: Uuid = row.get("module_id");
                for c in &cases {
                    sqlx::query(
                        "insert into test_cases (project_id, code, title, level, platforms, preconditions, steps, expected, data, refs, note, should_fail, status, module_id, origin, review_status)
                         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,false,'active',$12,'generated','draft')",
                    )
                    .bind(project_id)
                    .bind(&c.code)
                    .bind(&c.title)
                    .bind(&c.level)
                    .bind(&c.platforms)
                    .bind(serde_json::json!(c.preconditions))
                    .bind(serde_json::json!(c.steps))
                    .bind(serde_json::to_value(&c.expected).expect("可序列化"))
                    .bind(&c.data)
                    .bind(serde_json::json!(c.refs))
                    .bind(&c.note)
                    .bind(module_id)
                    .execute(&mut *tx)
                    .await?;
                }
                let codes: Vec<String> = cases.iter().map(|c| c.code.clone()).collect();
                let briefs = cases
                    .iter()
                    .map(|c| CaseBrief {
                        code: c.code.clone(),
                        title: c.title.clone(),
                        preconditions: c.preconditions.clone(),
                        steps: c.steps.clone(),
                        expected: c.expected.iter().map(|e| e.text.clone()).collect(),
                    })
                    .collect();
                ("finished", format!("生成了 {} 条用例（草稿，待复核）", codes.len()), codes, briefs)
            }
        }
    };
    sqlx::query("update casegen_requests set status = $2, detail = $3, finished_at = now() where id = $1")
        .bind(id)
        .bind(status)
        .bind(&detail)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Finished { status, detail, codes, cases: briefs })
}

pub(crate) async fn api_claim(State(state): State<ApiState>, headers: HeaderMap) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    let node_id = crate::node_identity(&state.app, &headers).await.map_err(|status| (status, String::new()))?;
    match claim(state.app.pool(), node_id).await.map_err(|e| api_error(e, "领取生成请求失败"))? {
        Some(claimed) => Ok(Json(claimed).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

pub(crate) async fn api_finish(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<FinishInput>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let node_id = crate::node_identity(&state.app, &headers).await.map_err(|status| (status, String::new()))?;
    let id: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个生成请求".to_string()))?;
    let Json(input) = body.map_err(|_| (StatusCode::BAD_REQUEST, "请求格式不对，或内容太大".to_string()))?;
    let done = finish(state.app.pool(), node_id, id, &input).await.map_err(|e| api_error(e, "回报生成结果失败"))?;
    Ok(Json(serde_json::json!({ "status": done.status, "detail": done.detail, "codes": done.codes, "cases": done.cases })))
}

// ---------------------------------------------------------------- 复核：评审员给意见，人来确认

pub const MAX_NOTE_CHARS: usize = 600;

#[derive(Debug, Deserialize)]
pub struct ReviewItem {
    pub code: String,
    /// `ok`：评审员认为文档里能找到依据、预期可判定；`needs_human`：有疑问或无法判断
    pub verdict: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewInput {
    /// 评审员是否跑完了。没跑成（没有评审员角色、额度用完、输出不是合法 JSON）时为 false，用例一律转待人工确认
    pub ok: bool,
    pub detail: String,
    #[serde(default)]
    pub reviews: Vec<ReviewItem>,
}

pub struct Reviewed {
    pub reviewed_ok: Vec<String>,
    pub needs_human: Vec<String>,
}

fn clip_note(text: &str) -> String {
    text.trim().chars().take(MAX_NOTE_CHARS).collect()
}

/// 节点回报评审员的意见：只更新**这次请求生成的、仍是草稿**的用例。
/// **评审员的通过只是意见（`reviewed_ok`），不会让用例能执行**，要人确认（规则 5：不强行通过）；
/// 没给出结论的、结论不认识的、评审员没跑成的，一律 `needs_human`。
pub async fn review(pool: &PgPool, node_id: Uuid, id: Uuid, input: &ReviewInput) -> Res<Reviewed> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("select node_id, status, project_id, module_id from casegen_requests where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ModuleError::NotFound("没有这个生成请求".into()))?;
    if row.get::<Uuid, _>("node_id") != node_id {
        return Err(ModuleError::Conflict("这个生成请求不是你的节点领走的".into()));
    }
    if row.get::<String, _>("status") != "finished" {
        return Err(ModuleError::Conflict("这个生成请求没有生成成功，没有可复核的用例".into()));
    }
    let project_id: Uuid = row.get("project_id");
    let module_id: Uuid = row.get("module_id");
    let short = &id.simple().to_string()[..6];
    let like = format!("GEN-{short}-%");
    let codes: Vec<String> = sqlx::query_scalar("select code from test_cases where project_id = $1 and module_id = $2 and origin = 'generated' and review_status = 'draft' and code like $3 order by code")
        .bind(project_id)
        .bind(module_id)
        .bind(&like)
        .fetch_all(&mut *tx)
        .await?;
    let mut out = Reviewed { reviewed_ok: vec![], needs_human: vec![] };
    for code in &codes {
        let item = input.reviews.iter().find(|r| &r.code == code);
        let (status, note) = match (input.ok, item) {
            (false, _) => ("needs_human", format!("评审员没有跑成，需要人工确认：{}", clip_note(&input.detail))),
            (true, None) => ("needs_human", "评审员没有给出这条用例的结论，需要人工确认".to_string()),
            (true, Some(r)) if r.verdict == "ok" => ("reviewed_ok", clip_note(&r.reason)),
            (true, Some(r)) if r.verdict == "needs_human" => ("needs_human", clip_note(&r.reason)),
            (true, Some(_)) => ("needs_human", "评审员给出的结论无法识别，需要人工确认".to_string()),
        };
        sqlx::query("update test_cases set review_status = $3, review_note = $4, reviewed_at = now() where project_id = $1 and code = $2")
            .bind(project_id)
            .bind(code)
            .bind(status)
            .bind(&note)
            .execute(&mut *tx)
            .await?;
        if status == "reviewed_ok" { out.reviewed_ok.push(code.clone()) } else { out.needs_human.push(code.clone()) }
    }
    let detail: String = input.detail.chars().take(1800).collect();
    sqlx::query("update casegen_requests set review_detail = $2 where id = $1").bind(id).bind(detail).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(out)
}

pub(crate) async fn api_review(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<ReviewInput>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let node_id = crate::node_identity(&state.app, &headers).await.map_err(|status| (status, String::new()))?;
    let id: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个生成请求".to_string()))?;
    let Json(input) = body.map_err(|_| (StatusCode::BAD_REQUEST, "请求格式不对，或内容太大".to_string()))?;
    let done = review(state.app.pool(), node_id, id, &input).await.map_err(|e| api_error(e, "回报复核意见失败"))?;
    Ok(Json(serde_json::json!({ "reviewed_ok": done.reviewed_ok, "needs_human": done.needs_human })))
}

#[derive(Debug, Serialize)]
pub struct ModuleCase {
    pub code: String,
    pub title: String,
    /// 运行环境：app / h5 / browser（由 platforms 推导，和 CSV 里的「运行环境」一致）
    pub run_env: String,
    pub level: String,
    pub preconditions: Vec<String>,
    pub origin: String,
    pub review_status: String,
    pub review_note: String,
    pub steps: Vec<String>,
    pub expected: Vec<String>,
    /// 这条用例最近一次执行（还没执行过为 null）
    pub latest_run: Option<LatestRun>,
}

#[derive(Debug, Serialize)]
pub struct LatestRun {
    pub run_id: Uuid,
    pub task_id: Uuid,
    pub status: String,
    pub final_result: Option<String>,
    pub blocked_reason: Option<String>,
    pub queued_at: DateTime<Utc>,
}

pub async fn module_cases(pool: &PgPool, module_id: Uuid) -> Res<Vec<ModuleCase>> {
    let exists: bool = sqlx::query_scalar("select exists(select 1 from modules where id = $1)").bind(module_id).fetch_one(pool).await?;
    if !exists {
        return Err(ModuleError::NotFound("没有这个模块".into()));
    }
    let rows = sqlx::query(
        "select c.code, c.title, c.level, c.platforms, c.preconditions, c.origin, c.review_status, c.review_note, c.steps, c.expected,
                r.id as run_id, r.task_id, r.status as run_status, r.final_result, r.blocked_reason, r.queued_at
         from test_cases c
         left join lateral (select * from runs x where x.case_id = c.id order by x.seq desc limit 1) r on true
         where c.module_id = $1 order by c.code",
    )
        .bind(module_id)
        .fetch_all(pool)
        .await?;
    let strings = |v: serde_json::Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default() };
    Ok(rows
        .iter()
        .map(|r| ModuleCase {
            code: r.get("code"),
            title: r.get("title"),
            run_env: assets::run_env_of(&r.get::<Vec<String>, _>("platforms")).to_string(),
            level: r.get("level"),
            preconditions: strings(r.get("preconditions")),
            origin: r.get("origin"),
            review_status: r.get("review_status"),
            review_note: r.get("review_note"),
            steps: strings(r.get("steps")),
            expected: r.get::<serde_json::Value, _>("expected").as_array().map(|a| a.iter().filter_map(|e| e["text"].as_str().map(str::to_owned)).collect()).unwrap_or_default(),
            latest_run: r.get::<Option<Uuid>, _>("run_id").map(|run_id| LatestRun {
                run_id,
                task_id: r.get("task_id"),
                status: r.get("run_status"),
                final_result: r.get("final_result"),
                blocked_reason: r.get("blocked_reason"),
                queued_at: r.get("queued_at"),
            }),
        })
        .collect())
}

pub(crate) async fn api_module_cases(State(state): State<ApiState>, Path(id): Path<String>) -> Result<Json<Vec<ModuleCase>>, ApiError> {
    let id: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个模块".to_string()))?;
    module_cases(state.app.pool(), id).await.map(Json).map_err(|e| api_error(e, "查询模块用例失败"))
}

/// 发起模块生成请求的人可以对尚未批准的生成用例作出决定。
async fn decider(state: &ApiState, headers: &HeaderMap, module_id: Uuid) -> Result<Uuid, ApiError> {
    let user = match users::browser_user(state.app.pool(), headers).await.map_err(|e| internal("校验确认人失败", e))? {
        BrowserUser::Identified(user) => user,
        BrowserUser::LoginRequired => return Err((StatusCode::UNAUTHORIZED, users::NEED_LOGIN_MESSAGE.into())),
        _ => return Err((StatusCode::FORBIDDEN, users::NEED_NAME_MESSAGE.into())),
    };
    let requested: bool = sqlx::query_scalar("select exists(select 1 from casegen_requests where module_id = $1 and requested_by = $2)")
        .bind(module_id)
        .bind(user)
        .fetch_one(state.app.pool())
        .await
        .map_err(|e| internal("校验确认人失败", e))?;
    if !requested {
        return Err((StatusCode::FORBIDDEN, "只有发起这个模块生成请求的人能确认或拒绝它的用例".into()));
    }
    Ok(user)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecideInput {
    /// `approve` 批准（才能执行）/ `reject` 拒绝
    decision: String,
    #[serde(default)]
    note: String,
}

/// 逐条批准或拒绝。需要人确认的（`needs_human`）也能批准——这是人的决定，评审员的疑问写在备注里留痕。
pub(crate) async fn api_decide(
    State(state): State<ApiState>,
    Path((id, code)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<DecideInput>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let module_id: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个模块".to_string()))?;
    let Json(input) = body.map_err(|_| (StatusCode::BAD_REQUEST, "请求格式不对".to_string()))?;
    let status = match input.decision.as_str() {
        "approve" => "approved",
        "reject" => "rejected",
        _ => return Err((StatusCode::BAD_REQUEST, "decision 只能是 approve 或 reject".to_string())),
    };
    let user = decider(&state, &headers, module_id).await?;
    let note = clip_note(&input.note);
    let pool = state.app.pool();
    let current: Option<String> = sqlx::query_scalar("select review_status from test_cases where module_id = $1 and code = $2 and origin = 'generated'")
        .bind(module_id)
        .bind(&code)
        .fetch_optional(pool)
        .await
        .map_err(|e| internal("查询用例失败", e))?;
    let Some(current) = current else { return Err((StatusCode::NOT_FOUND, "这个模块里没有这条生成的用例".to_string())) };
    if current == "approved" && status == "approved" {
        return Ok(StatusCode::NO_CONTENT);
    }
    // 评审员说有疑问的用例，人批准时要写一句理由，留痕
    if current == "needs_human" && status == "approved" && note.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "评审员对这条用例有疑问，批准时请写一句备注说明为什么可以执行".to_string()));
    }
    let append = if note.is_empty() { String::new() } else { format!("（人工备注：{note}）") };
    sqlx::query(
        "update test_cases set review_status = $3, reviewed_by_user = $4, reviewed_at = now(),
                review_note = left(review_note || $5, $6)
         where module_id = $1 and code = $2 and origin = 'generated'",
    )
    .bind(module_id)
    .bind(&code)
    .bind(status)
    .bind(user)
    .bind(append)
    .bind(MAX_NOTE_CHARS as i32)
    .execute(pool)
    .await
    .map_err(|e| internal("更新用例失败", e))?;
    Ok(StatusCode::NO_CONTENT)
}

/// 一键批准评审员认为没问题的（`reviewed_ok`）。需要人确认的（`needs_human`）不在内，必须逐条处理。
pub(crate) async fn api_approve_reviewed(State(state): State<ApiState>, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<serde_json::Value>, ApiError> {
    let module_id: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个模块".to_string()))?;
    let user = decider(&state, &headers, module_id).await?;
    let result = sqlx::query("update test_cases set review_status = 'approved', reviewed_by_user = $2, reviewed_at = now() where module_id = $1 and origin = 'generated' and review_status = 'reviewed_ok'")
        .bind(module_id)
        .bind(user)
        .execute(state.app.pool())
        .await
        .map_err(|e| internal("批准用例失败", e))?;
    Ok(Json(serde_json::json!({ "approved": result.rows_affected() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nxxxx";

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn input() -> CasegenInput {
        CasegenInput {
            module: "语聊房".into(),
            doc: "# 需求\n用户可以进入语聊房。登录账号 {{account.username}}，密码 {{account.password}}。".into(),
            ui_links: vec!["https://example.com/design/abc?node=1".into()],
            screenshots: vec![ShotInput { data_b64: b64(PNG) }],
            node_id: Uuid::nil(),
            writer_role: None,
            reviewer_role: None,
        }
    }

    fn message(input: &CasegenInput) -> String {
        match validate(input) {
            Err(e) => e.to_string(),
            Ok(_) => "通过".into(),
        }
    }

    #[test]
    fn 必填项齐全才通过_缺哪项说哪项() {
        assert_eq!(message(&input()), "通过");
        let mut i = input();
        i.module = "  ".into();
        assert!(message(&i).contains("模块必填"));
        let mut i = input();
        i.doc = "  \n ".into();
        assert!(message(&i).contains("需求文档必填"));
        let mut i = input();
        i.ui_links.clear();
        assert!(message(&i).contains("UI 图链接必填"));
        let mut i = input();
        i.screenshots.clear();
        assert!(message(&i).contains("UI 区域截图必填"));
    }

    #[test]
    fn 链接只允许http和https_不带账号() {
        for ok in ["https://figma.com/file/x", "http://127.0.0.1:3000/a#b"] {
            assert!(check_link(ok).is_ok(), "{ok}");
        }
        for bad in ["javascript:alert(1)", "file:///etc/passwd", "ftp://a.com", "https://", "https://用户:密码@a.com/x", "https://a.com/a b", "a.com/x", ""] {
            assert!(check_link(bad).is_err(), "应拒绝：{bad}");
        }
    }

    #[test]
    fn 截图以文件头为准且限制大小和数量() {
        let mut i = input();
        i.screenshots = vec![ShotInput { data_b64: b64(b"GIF89a....") }];
        assert!(message(&i).contains("不是 PNG 或 JPEG"));
        i.screenshots = vec![ShotInput { data_b64: "不是base64!!".into() }];
        assert!(message(&i).contains("base64"));
        let mut big = PNG.to_vec();
        big.resize(MAX_SHOT_BYTES + 1, 0);
        i.screenshots = vec![ShotInput { data_b64: b64(&big) }];
        assert!(message(&i).contains("超过"));
        i.screenshots = (0..=MAX_SHOTS).map(|_| ShotInput { data_b64: b64(PNG) }).collect();
        assert!(message(&i).contains("最多"));
        i.screenshots = vec![ShotInput { data_b64: b64(b"\xff\xd8\xff\xe0jpeg") }];
        assert!(validate(&i).is_ok(), "JPEG 应通过");
    }

    #[test]
    fn 截图最多一百张_合计不超过一百兆() {
        assert_eq!(MAX_SHOTS, 100);
        // 100 张小图：通过
        let mut i = input();
        i.screenshots = (0..MAX_SHOTS).map(|_| ShotInput { data_b64: b64(PNG) }).collect();
        assert!(validate(&i).is_ok(), "100 张应通过");
        // 每张都没超过单张上限（3MB），但 40 张 × 3MB = 120MB 超过合计上限
        let mut shot = PNG.to_vec();
        shot.resize(MAX_SHOT_BYTES, 0);
        let one = ShotInput { data_b64: b64(&shot) };
        i.screenshots = (0..40).map(|_| ShotInput { data_b64: one.data_b64.clone() }).collect();
        let text = message(&i);
        assert!(text.contains("合计") && text.contains("100 MB"), "{text}");
        // 30 张 × 3MB = 90MB：在合计上限内
        i.screenshots.truncate(30);
        assert!(validate(&i).is_ok(), "90MB 应通过");
    }

    #[test]
    fn 文档里的明文密码被拒_占位符放行_错误里不回显值() {
        for bad in ["测试账号密码：hunter2", "password = hunter2", "Passwd: `hunter2`", "pwd：hunter2 登录"] {
            let mut i = input();
            i.doc = format!("# 需求\n{bad}\n");
            let m = message(&i);
            assert!(m.contains("疑似有明文密码") && m.contains("第 2 行"), "{bad} → {m}");
            assert!(!m.contains("hunter2"), "错误里不能回显密码：{m}");
        }
        for ok in ["密码必须是 8 位以上", "密码：{{account.password}}", "password: {{account.password}}", "忘记密码入口在登录页"] {
            let mut i = input();
            i.doc = format!("# 需求\n{ok}\n");
            assert_eq!(message(&i), "通过", "不应误伤：{ok}");
        }
    }

    #[test]
    fn 内容相同就是同一份文档_换行符和行尾空格不影响() {
        let a = check_doc("# 需求\n用户可以进入语聊房。\n").unwrap();
        let b = check_doc("\r\n# 需求  \r\n用户可以进入语聊房。   \r\n\r\n").unwrap();
        let c = check_doc("# 需求\r用户可以进入语聊房。").unwrap();
        assert_eq!((a.as_str(), b.as_str(), c.as_str()), ("# 需求\n用户可以进入语聊房。", "# 需求\n用户可以进入语聊房。", "# 需求\n用户可以进入语聊房。"));
        // 真的不一样（多一个字）才是另一份
        assert_ne!(a, check_doc("# 需求\n用户可以进入语聊房吗。").unwrap());
        // 行内的空白是内容的一部分，不动
        assert_ne!(check_doc("a  b").unwrap(), check_doc("a b").unwrap());
    }

    #[test]
    fn 文档过长被拒() {
        let mut i = input();
        i.doc = "需".repeat(MAX_DOC_CHARS + 1);
        assert!(message(&i).contains("太长"));
    }

    const GOOD_CASE: &str = "id: G1\ntitle: 进入语聊房\nlevel: 简单流程\nplatform: [android]\nsteps:\n  - 点击首页的语聊房入口\nexpected:\n  - 出现语聊房页面\n";

    #[test]
    fn 生成的用例拆分_去围栏_编号由平台分配() {
        let id = Uuid::parse_str("abcdef12-0000-0000-0000-000000000000").unwrap();
        let text = format!("```yaml\n{GOOD_CASE}---\n{}\n```\n", GOOD_CASE.replace("G1", "AnyId-7").replace("进入语聊房", "退出语聊房"));
        let cases = parse_generated(id, &text).unwrap();
        assert_eq!(cases.iter().map(|c| c.code.as_str()).collect::<Vec<_>>(), ["GEN-abcdef-01", "GEN-abcdef-02"]);
        assert_eq!(cases[1].title, "退出语聊房");
        assert!(cases.iter().all(|c| !c.should_fail));
    }

    #[test]
    fn 生成的用例任何一条不合格整批拒收_错误不回显密码() {
        let id = Uuid::new_v4();
        // 没有预期
        let bad = "id: G2\ntitle: 没有预期\nsteps:\n  - 点击\nexpected: []\n";
        let err = parse_generated(id, &format!("{GOOD_CASE}---\n{bad}")).unwrap_err();
        assert!(err.contains("第 2 条用例") && err.contains("expected"), "{err}");
        // data 里写了明文账号
        let plain = "id: G3\ntitle: 登录\nsteps:\n  - 登录\nexpected:\n  - 登录成功\ndata:\n  password: hunter2\n";
        let err = parse_generated(id, plain).unwrap_err();
        assert!(!err.contains("hunter2"), "{err}");
        // 步骤文字里的明文密码
        let in_steps = "id: G4\ntitle: 登录\nsteps:\n  - 输入密码：hunter2\nexpected:\n  - 登录成功\n";
        let err = parse_generated(id, in_steps).unwrap_err();
        assert!(err.contains("明文密码") && !err.contains("hunter2"), "{err}");
        // 占位符放行
        let ok = "id: G5\ntitle: 登录\nsteps:\n  - 输入密码：{{account.password}}\nexpected:\n  - 登录成功\n";
        assert!(parse_generated(id, ok).is_ok());
        // 不能是「故意失败」级别；空输出；太多
        let negative = GOOD_CASE.replace("简单流程", "故意失败");
        assert!(parse_generated(id, &negative).unwrap_err().contains("故意失败"));
        assert!(parse_generated(id, "  \n").unwrap_err().contains("没有输出"));
        let many = vec![GOOD_CASE; MAX_CASES + 1].join("---\n");
        assert!(parse_generated(id, &many).unwrap_err().contains("超过上限"));
    }
}
