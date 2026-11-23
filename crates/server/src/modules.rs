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

pub const MAX_DOC_CHARS: usize = 200_000;
pub const MAX_LINKS: usize = 5;
pub const MAX_LINK_CHARS: usize = 2000;
pub const MAX_SHOTS: usize = 6;
pub const MAX_SHOT_BYTES: usize = 3 * 1024 * 1024;
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
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

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

pub fn check_doc(doc: &str) -> Res<String> {
    let doc = doc.trim();
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
    let Json(input) = body.map_err(|_| (StatusCode::BAD_REQUEST, "请求格式无效，或内容太大（截图合计不要超过约 18MB）".to_string()))?;
    // 先校验必填和格式，再校验身份和节点属主：缺项的提示不依赖库
    let validated = validate(&input).map_err(|e| api_error(e, "校验失败"))?;
    let token = headers.get("x-user-token").and_then(|h| h.to_str().ok()).unwrap_or_default();
    let user = crate::api::owner_check(&state, token, input.node_id).await?;
    if !state.app.node_exists(input.node_id).await.map_err(|e| internal("查询节点失败", e))? {
        return Err((StatusCode::NOT_FOUND, "没有这个节点".to_string()));
    }
    let created = create_request(state.app.pool(), &project, user, &input, &validated).await.map_err(|e| api_error(e, "创建生成请求失败"))?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "request_id": created.request_id, "module_id": created.module_id, "doc_id": created.doc_id }))))
}

pub(crate) async fn api_request_detail(State(state): State<ApiState>, Path(id): Path<String>) -> Result<Json<RequestDetail>, ApiError> {
    let id: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个生成请求".to_string()))?;
    request_detail(state.app.pool(), id).await.map(Json).map_err(|e| api_error(e, "查询生成请求失败"))
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
    fn 文档过长被拒() {
        let mut i = input();
        i.doc = "需".repeat(MAX_DOC_CHARS + 1);
        assert!(message(&i).contains("太长"));
    }
}
