//! 参数配置（只读）：一级配置类型、二级配置项。所有人能看，写入接口在 P2 做（只给管理员）。

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use axum::http::HeaderMap;
use uuid::Uuid;

use crate::api::ApiState;
use crate::internal;
use crate::users::{self, BrowserUser};

type ApiError = (StatusCode, String);

const DEFAULT_PAGE_SIZE: i64 = 10;
const MAX_PAGE_SIZE: i64 = 100;

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    /// 名称，模糊匹配（仅一级列表）
    name: Option<String>,
    /// 编码，模糊匹配（仅一级列表）
    code: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
}

#[derive(Serialize)]
struct Page<T> {
    total: i64,
    page: i64,
    page_size: i64,
    rows: Vec<T>,
    /// 当前请求的人能不能改配置（只有管理员）；页面据此决定按钮能不能点
    can_write: bool,
}

#[derive(Serialize)]
struct ConfigType {
    id: i64,
    name: String,
    code: String,
    remark: String,
    status: String,
    item_count: i64,
    updated_at: DateTime<Utc>,
    updated_by: Option<String>,
}

#[derive(Serialize)]
struct ConfigItem {
    id: i64,
    name: String,
    code: String,
    value: String,
    label: String,
    group_name: String,
    sort: i32,
    remark: String,
    status: String,
    updated_at: DateTime<Utc>,
    updated_by: Option<String>,
}

#[derive(Serialize)]
struct TypeDetail {
    #[serde(rename = "type")]
    config_type: ConfigType,
    items: Page<ConfigItem>,
    can_write: bool,
}

/// 页码从 1 开始；每页条数限制在 1..=100，避免一次拉太多。
fn paging(query: &ListQuery) -> (i64, i64) {
    let size = query.page_size.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let page = query.page.unwrap_or(1).max(1);
    (page, size)
}

/// 把用户输入当普通文本做模糊匹配：转义 LIKE 的通配符。
fn like_pattern(input: &str) -> String {
    let escaped = input.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%{escaped}%")
}

fn type_from_row(row: &sqlx::postgres::PgRow) -> ConfigType {
    ConfigType {
        id: row.get("id"),
        name: row.get("name"),
        code: row.get("code"),
        remark: row.get("remark"),
        status: row.get("status"),
        item_count: row.get("item_count"),
        updated_at: row.get("updated_at"),
        updated_by: row.get("updated_by"),
    }
}

/// sqlx 不接受拼出来的 SQL，所以公共部分用宏在编译期拼成字面量。
macro_rules! type_select {
    ($tail:literal) => {
        concat!(
            "select t.id, t.name, t.code, t.remark, t.status, t.updated_at, t.updated_by,
                (select count(*) from config_items i where i.type_id = t.id) as item_count
             from config_types t ",
            $tail
        )
    };
}

/// 一级列表：按名称、编码搜索，分页。
pub(crate) async fn api_list_types(State(state): State<ApiState>, headers: HeaderMap, Query(query): Query<ListQuery>) -> Result<impl IntoResponse, ApiError> {
    let (page, size) = paging(&query);
    let can_write = admin_user(&state, &headers).await?.is_some();
    let name = query.name.as_deref().map(like_pattern);
    let code = query.code.as_deref().map(like_pattern);
    let pool = state.app.pool();
    let total: i64 = sqlx::query_scalar("select count(*) from config_types t where ($1::text is null or t.name ilike $1) and ($2::text is null or t.code ilike $2)")
        .bind(&name)
        .bind(&code)
        .fetch_one(pool)
        .await
        .map_err(|e| internal("查询配置类型失败", e))?;
    let rows = sqlx::query(type_select!("where ($1::text is null or t.name ilike $1) and ($2::text is null or t.code ilike $2) order by t.id limit $3 offset $4"))
        .bind(&name)
        .bind(&code)
        .bind(size)
        .bind((page - 1) * size)
        .fetch_all(pool)
        .await
        .map_err(|e| internal("查询配置类型失败", e))?;
    let rows = rows.iter().map(type_from_row).collect();
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(Page { total, page, page_size: size, rows, can_write })))
}

/// 二级详情：一个配置类型和它下面的配置项（按排序、编号），分页。
pub(crate) async fn api_type_detail(State(state): State<ApiState>, headers: HeaderMap, Path(code): Path<String>, Query(query): Query<ListQuery>) -> Result<impl IntoResponse, ApiError> {
    let (page, size) = paging(&query);
    let can_write = admin_user(&state, &headers).await?.is_some();
    let pool = state.app.pool();
    let row = sqlx::query(type_select!("where t.code = $1"))
        .bind(&code)
        .fetch_optional(pool)
        .await
        .map_err(|e| internal("查询配置类型失败", e))?
        .ok_or((StatusCode::NOT_FOUND, "没有这个配置类型".to_string()))?;
    let config_type = type_from_row(&row);
    let rows = sqlx::query(
        "select id, name, code, value, label, group_name, sort, remark, status, updated_at, updated_by
         from config_items where type_id = $1 order by sort, id limit $2 offset $3",
    )
    .bind(config_type.id)
    .bind(size)
    .bind((page - 1) * size)
    .fetch_all(pool)
    .await
    .map_err(|e| internal("查询配置项失败", e))?;
    let items = rows
        .iter()
        .map(|r| ConfigItem {
            id: r.get("id"),
            name: r.get("name"),
            code: r.get("code"),
            value: r.get("value"),
            label: r.get("label"),
            group_name: r.get("group_name"),
            sort: r.get("sort"),
            remark: r.get("remark"),
            status: r.get("status"),
            updated_at: r.get("updated_at"),
            updated_by: r.get("updated_by"),
        })
        .collect();
    let total = config_type.item_count;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(TypeDetail { config_type, items: Page { total, page, page_size: size, rows: items, can_write }, can_write })))
}

// ---------------------------------------------------------------- 写入（只给管理员）

const NEED_ADMIN: &str = "只有管理员能改配置：请先用管理员名字登录";

/// 请求的人是不是管理员：名字登记过、有密码（靠会话令牌确认身份）、`is_admin` 为真。
/// 没有密码的名字只靠自觉，不能当管理员。返回管理员的名字。
async fn admin_user(state: &ApiState, headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let pool = state.app.pool();
    let BrowserUser::Identified(user) = users::browser_user(pool, headers).await.map_err(|e| internal("校验用户失败", e))? else {
        return Ok(None);
    };
    let row: Option<(String,)> = sqlx::query_as("select name from users where id = $1 and is_admin and password_hash is not null")
        .bind::<Uuid>(user)
        .fetch_optional(pool)
        .await
        .map_err(|e| internal("校验管理员失败", e))?;
    Ok(row.map(|(name,)| name))
}

pub(crate) async fn require_admin(state: &ApiState, headers: &HeaderMap) -> Result<String, ApiError> {
    admin_user(state, headers).await?.ok_or((StatusCode::FORBIDDEN, NEED_ADMIN.to_string()))
}

fn bad(message: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}

/// 配置值里不能有密码或 API Key（项目规则）。只看特征，宁可误拦：误拦了改个说法就行，漏了就是泄露。
pub(crate) fn looks_secret(text: &str) -> bool {
    let lower = text.to_lowercase();
    const WORDS: [&str; 9] = ["password", "passwd", "secret", "api_key", "apikey", "api-key", "access_token", "private_key", "密码"];
    if WORDS.iter().any(|w| lower.contains(w)) {
        return true;
    }
    // sk-xxxxxxxx…（常见的 API Key 形态）和 AWS 的 AKIA…
    let bytes = text.as_bytes();
    let run = |start: usize, ok: fn(u8) -> bool| bytes[start..].iter().take_while(|b| ok(**b)).count();
    (0..bytes.len()).any(|i| {
        (bytes[i..].starts_with(b"sk-") && run(i + 3, |b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') >= 16)
            || (bytes[i..].starts_with(b"AKIA") && run(i + 4, |b| b.is_ascii_uppercase() || b.is_ascii_digit()) >= 16)
    })
}

fn valid_code(code: &str) -> bool {
    // export、import 是接口路径，不能当类型编码用（会和 /api/configs/{code} 撞）
    if code == "export" || code == "import" {
        return false;
    }
    let mut chars = code.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && code.len() <= 50
        && code.len() >= 2
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn check_text(label: &str, text: &str, max: usize, required: bool) -> Result<String, ApiError> {
    let text = text.trim();
    if required && text.is_empty() {
        return Err(bad(format!("请填写{label}")));
    }
    if text.chars().count() > max {
        return Err(bad(format!("{label}不超过 {max} 个字")));
    }
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return Err(bad(format!("{label}不能含控制字符")));
    }
    if looks_secret(text) {
        return Err(bad(format!("{label}里不能出现密码或 API Key")));
    }
    Ok(text.to_string())
}

fn check_status(status: &str) -> Result<&str, ApiError> {
    match status {
        "normal" | "disabled" => Ok(if status == "normal" { "normal" } else { "disabled" }),
        _ => Err(bad("状态只能是正常或停用")),
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: Result<Json<T>, axum::extract::rejection::JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(v)| v).map_err(|_| bad("请求格式无效"))
}

async fn type_id(state: &ApiState, code: &str) -> Result<i64, ApiError> {
    sqlx::query_scalar("select id from config_types where code = $1")
        .bind(code)
        .fetch_optional(state.app.pool())
        .await
        .map_err(|e| internal("查询配置类型失败", e))?
        .ok_or((StatusCode::NOT_FOUND, "没有这个配置类型".to_string()))
}

#[derive(Deserialize)]
pub(crate) struct TypeInput {
    name: String,
    /// 只在新增时使用；修改时编码不能改（链接和程序都靠它找配置）
    #[serde(default)]
    code: String,
    #[serde(default)]
    remark: String,
    #[serde(default = "normal")]
    status: String,
}

fn normal() -> String {
    "normal".into()
}

pub(crate) async fn api_create_type(State(state): State<ApiState>, headers: HeaderMap, body: Result<Json<TypeInput>, axum::extract::rejection::JsonRejection>) -> Result<impl IntoResponse, ApiError> {
    let admin = require_admin(&state, &headers).await?;
    let input = parse(body)?;
    let name = check_text("名称", &input.name, 50, true)?;
    let remark = check_text("备注", &input.remark, 200, false)?;
    let code = input.code.trim().to_string();
    if !valid_code(&code) {
        return Err(bad("编码只能用小写字母、数字、下划线，以字母开头，2 到 50 位"));
    }
    let status = check_status(&input.status)?;
    let created = sqlx::query("insert into config_types (name, code, remark, status, updated_by) values ($1,$2,$3,$4,$5)")
        .bind(&name).bind(&code).bind(&remark).bind(status).bind(&admin)
        .execute(state.app.pool())
        .await;
    match created {
        Ok(_) => Ok((StatusCode::CREATED, Json(serde_json::json!({ "code": code })))),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err((StatusCode::CONFLICT, "这个编码已经有了".to_string())),
        Err(e) => Err(internal("新增配置类型失败", e)),
    }
}

pub(crate) async fn api_update_type(State(state): State<ApiState>, headers: HeaderMap, Path(code): Path<String>, body: Result<Json<TypeInput>, axum::extract::rejection::JsonRejection>) -> Result<StatusCode, ApiError> {
    let admin = require_admin(&state, &headers).await?;
    let input = parse(body)?;
    let name = check_text("名称", &input.name, 50, true)?;
    let remark = check_text("备注", &input.remark, 200, false)?;
    let status = check_status(&input.status)?;
    let done = sqlx::query("update config_types set name=$2, remark=$3, status=$4, updated_at=now(), updated_by=$5 where code=$1")
        .bind(&code).bind(&name).bind(&remark).bind(status).bind(&admin)
        .execute(state.app.pool())
        .await
        .map_err(|e| internal("修改配置类型失败", e))?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "没有这个配置类型".to_string()));
    }
    push_params(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn api_delete_type(State(state): State<ApiState>, headers: HeaderMap, Path(code): Path<String>) -> Result<StatusCode, ApiError> {
    require_admin(&state, &headers).await?;
    let id = type_id(&state, &code).await?;
    let items: i64 = sqlx::query_scalar("select count(*) from config_items where type_id = $1")
        .bind(id)
        .fetch_one(state.app.pool())
        .await
        .map_err(|e| internal("查询配置项失败", e))?;
    if items > 0 {
        return Err((StatusCode::CONFLICT, format!("请先清空下面的 {items} 条配置项再删除")));
    }
    sqlx::query("delete from config_types where id = $1")
        .bind(id)
        .execute(state.app.pool())
        .await
        .map_err(|e| internal("删除配置类型失败", e))?;
    push_params(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(crate) struct ItemInput {
    name: String,
    /// 只在新增时使用；修改时编码不能改
    #[serde(default)]
    code: String,
    value: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    group_name: String,
    #[serde(default)]
    sort: i32,
    #[serde(default)]
    remark: String,
    #[serde(default = "normal")]
    status: String,
}

struct CleanItem {
    name: String,
    value: String,
    label: String,
    group_name: String,
    remark: String,
}

fn clean_item(input: &ItemInput) -> Result<CleanItem, ApiError> {
    Ok(CleanItem {
        name: check_text("名称", &input.name, 50, true)?,
        value: check_text("值", &input.value, 2000, true)?,
        label: check_text("标签", &input.label, 32, false)?,
        group_name: check_text("分组", &input.group_name, 32, false)?,
        remark: check_text("备注", &input.remark, 200, false)?,
    })
}

pub(crate) async fn api_create_item(State(state): State<ApiState>, headers: HeaderMap, Path(code): Path<String>, body: Result<Json<ItemInput>, axum::extract::rejection::JsonRejection>) -> Result<impl IntoResponse, ApiError> {
    let admin = require_admin(&state, &headers).await?;
    let input = parse(body)?;
    let clean = clean_item(&input)?;
    let item_code = input.code.trim().to_string();
    if !valid_code(&item_code) {
        return Err(bad("编码只能用小写字母、数字、下划线，以字母开头，2 到 50 位"));
    }
    let status = check_status(&input.status)?;
    check_known_value(&code, &item_code, &clean.value)?;
    let id = type_id(&state, &code).await?;
    let created = sqlx::query_scalar::<_, i64>(
        "insert into config_items (type_id, name, code, value, label, group_name, sort, remark, status, updated_by)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) returning id",
    )
    .bind(id).bind(&clean.name).bind(&item_code).bind(&clean.value).bind(&clean.label).bind(&clean.group_name)
    .bind(input.sort).bind(&clean.remark).bind(status).bind(&admin)
    .fetch_one(state.app.pool())
    .await;
    match created {
        Ok(item) => {
            push_params(&state).await;
            Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": item }))))
        }
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err((StatusCode::CONFLICT, "这个类型下已经有同编码的配置项".to_string())),
        Err(e) => Err(internal("新增配置项失败", e)),
    }
}

pub(crate) async fn api_update_item(State(state): State<ApiState>, headers: HeaderMap, Path((code, item)): Path<(String, i64)>, body: Result<Json<ItemInput>, axum::extract::rejection::JsonRejection>) -> Result<StatusCode, ApiError> {
    let admin = require_admin(&state, &headers).await?;
    let input = parse(body)?;
    let clean = clean_item(&input)?;
    let status = check_status(&input.status)?;
    let id = type_id(&state, &code).await?;
    let item_code: String = sqlx::query_scalar("select code from config_items where id = $2 and type_id = $1")
        .bind(id).bind(item)
        .fetch_optional(state.app.pool())
        .await
        .map_err(|e| internal("查询配置项失败", e))?
        .ok_or((StatusCode::NOT_FOUND, "没有这个配置项".to_string()))?;
    check_known_value(&code, &item_code, &clean.value)?;
    if status != "normal" {
        guard_identity_in_use(&state, &code, &item_code).await?;
    }
    let done = sqlx::query(
        "update config_items set name=$3, value=$4, label=$5, group_name=$6, sort=$7, remark=$8, status=$9, updated_at=now(), updated_by=$10
         where id=$2 and type_id=$1",
    )
    .bind(id).bind(item).bind(&clean.name).bind(&clean.value).bind(&clean.label).bind(&clean.group_name)
    .bind(input.sort).bind(&clean.remark).bind(status).bind(&admin)
    .execute(state.app.pool())
    .await
    .map_err(|e| internal("修改配置项失败", e))?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "没有这个配置项".to_string()));
    }
    push_params(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

/// 「账号身份」类型下的身份还被账号或用例用着时，不允许删除或停用，否则账号和用例会悬空、永远领不到号。
async fn guard_identity_in_use(state: &ApiState, type_code: &str, item_code: &str) -> Result<(), ApiError> {
    if type_code != "account_identity" {
        return Ok(());
    }
    let (accounts, cases): (i64, i64) = sqlx::query_as(
        "select (select count(*) from test_accounts where identity = $1), (select count(*) from test_cases where account_identity = $1 and status != 'archived')",
    )
    .bind(item_code)
    .fetch_one(state.app.pool())
    .await
    .map_err(|e| internal("查询身份引用失败", e))?;
    if accounts > 0 || cases > 0 {
        return Err((StatusCode::CONFLICT, format!("这个身份还被 {accounts} 个账号、{cases} 条用例使用，请先改掉它们再删除或停用")));
    }
    Ok(())
}

pub(crate) async fn api_delete_item(State(state): State<ApiState>, headers: HeaderMap, Path((code, item)): Path<(String, i64)>) -> Result<StatusCode, ApiError> {
    require_admin(&state, &headers).await?;
    let id = type_id(&state, &code).await?;
    let item_code: Option<String> = sqlx::query_scalar("select code from config_items where id = $2 and type_id = $1")
        .bind(id).bind(item)
        .fetch_optional(state.app.pool())
        .await
        .map_err(|e| internal("查询配置项失败", e))?;
    if let Some(item_code) = &item_code {
        guard_identity_in_use(&state, &code, item_code).await?;
    }
    let done = sqlx::query("delete from config_items where id = $2 and type_id = $1")
        .bind(id).bind(item)
        .execute(state.app.pool())
        .await
        .map_err(|e| internal("删除配置项失败", e))?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "没有这个配置项".to_string()));
    }
    push_params(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- 配置接到运行逻辑

/// 每个岗位最多几个角色：读 `role_limit` 类型下的配置项（编码就是岗位名）。
/// 类型或配置项停用、没有、值不是 1 到 20 的整数时，用代码里的默认值，保证配置写坏了平台也能用。
pub(crate) async fn role_limit(pool: &sqlx::PgPool, slot: &str, default: usize) -> usize {
    let row: Result<Option<(String,)>, _> = sqlx::query_as(
        "select i.value from config_items i join config_types t on t.id = i.type_id
         where t.code = 'role_limit' and t.status = 'normal' and i.status = 'normal' and i.code = $1",
    )
    .bind(slot)
    .fetch_optional(pool)
    .await;
    match row {
        Ok(Some((value,))) => value.trim().parse::<usize>().ok().filter(|n| (1..=20).contains(n)).unwrap_or(default),
        _ => default,
    }
}

// ---------------------------------------------------------------- 导出和导入（JSON）

#[derive(Serialize, Deserialize)]
pub(crate) struct ExportItem {
    name: String,
    code: String,
    value: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    group_name: String,
    #[serde(default)]
    sort: i32,
    #[serde(default)]
    remark: String,
    #[serde(default = "normal")]
    status: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ExportType {
    name: String,
    code: String,
    #[serde(default)]
    remark: String,
    #[serde(default = "normal")]
    status: String,
    #[serde(default)]
    items: Vec<ExportItem>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ConfigDocument {
    types: Vec<ExportType>,
}

/// 导出全部配置（所有人能看，里面本来就不该有密码）。同时也是导入文件的模板。
pub(crate) async fn api_export(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    let pool = state.app.pool();
    let types = sqlx::query("select id, name, code, remark, status from config_types order by id")
        .fetch_all(pool)
        .await
        .map_err(|e| internal("导出配置失败", e))?;
    let mut out = Vec::new();
    for t in &types {
        let id: i64 = t.get("id");
        let items = sqlx::query("select name, code, value, label, group_name, sort, remark, status from config_items where type_id = $1 order by sort, id")
            .bind(id)
            .fetch_all(pool)
            .await
            .map_err(|e| internal("导出配置失败", e))?;
        out.push(ExportType {
            name: t.get("name"),
            code: t.get("code"),
            remark: t.get("remark"),
            status: t.get("status"),
            items: items
                .iter()
                .map(|r| ExportItem { name: r.get("name"), code: r.get("code"), value: r.get("value"), label: r.get("label"), group_name: r.get("group_name"), sort: r.get("sort"), remark: r.get("remark"), status: r.get("status") })
                .collect(),
        });
    }
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(ConfigDocument { types: out })))
}

#[derive(Deserialize)]
pub(crate) struct ImportRequest {
    /// `skip`：已有的编码跳过；`overwrite`：已有的编码用文件里的内容覆盖
    mode: String,
    document: ConfigDocument,
}

#[derive(Serialize, Default)]
struct ImportResult {
    created_types: u32,
    updated_types: u32,
    skipped_types: u32,
    created_items: u32,
    updated_items: u32,
    skipped_items: u32,
}

const MAX_IMPORT_TYPES: usize = 200;
const MAX_IMPORT_ITEMS: usize = 2000;

/// 导入：先把整个文件校验一遍（任何一条不合格就整体拒绝并说明是哪一条，什么都不写），
/// 再在一个事务里写入；写到一半失败也不会留下半份配置。只给管理员。
pub(crate) async fn api_import(State(state): State<ApiState>, headers: HeaderMap, body: Result<Json<ImportRequest>, axum::extract::rejection::JsonRejection>) -> Result<impl IntoResponse, ApiError> {
    let admin = require_admin(&state, &headers).await?;
    let request = parse(body)?;
    let overwrite = match request.mode.as_str() {
        "skip" => false,
        "overwrite" => true,
        _ => return Err(bad("导入方式只能是跳过或覆盖")),
    };
    let doc = request.document;
    if doc.types.is_empty() {
        return Err(bad("文件里没有配置类型"));
    }
    if doc.types.len() > MAX_IMPORT_TYPES || doc.types.iter().map(|t| t.items.len()).sum::<usize>() > MAX_IMPORT_ITEMS {
        return Err(bad(format!("一次最多导入 {MAX_IMPORT_TYPES} 个类型、{MAX_IMPORT_ITEMS} 个配置项")));
    }
    // 先全部校验，错误带上位置
    let mut seen_types = std::collections::HashSet::new();
    for (ti, t) in doc.types.iter().enumerate() {
        let at = |e: ApiError| bad(format!("第 {} 个类型「{}」：{}", ti + 1, t.code, e.1));
        check_text("名称", &t.name, 50, true).map_err(at)?;
        check_text("备注", &t.remark, 200, false).map_err(at)?;
        check_status(&t.status).map_err(at)?;
        if !valid_code(t.code.trim()) {
            return Err(at(bad("编码只能用小写字母、数字、下划线，以字母开头，2 到 50 位")));
        }
        if !seen_types.insert(t.code.trim().to_string()) {
            return Err(at(bad("类型编码在文件里重复了")));
        }
        let mut seen_items = std::collections::HashSet::new();
        for (ii, item) in t.items.iter().enumerate() {
            let at = |e: ApiError| bad(format!("类型「{}」第 {} 个配置项「{}」：{}", t.code, ii + 1, item.code, e.1));
            let as_input = ItemInput { name: item.name.clone(), code: item.code.clone(), value: item.value.clone(), label: item.label.clone(), group_name: item.group_name.clone(), sort: item.sort, remark: item.remark.clone(), status: item.status.clone() };
            clean_item(&as_input).map_err(at)?;
            check_known_value(t.code.trim(), item.code.trim(), &item.value).map_err(at)?;
            check_status(&item.status).map_err(at)?;
            if !valid_code(item.code.trim()) {
                return Err(at(bad("编码只能用小写字母、数字、下划线，以字母开头，2 到 50 位")));
            }
            if !seen_items.insert(item.code.trim().to_string()) {
                return Err(at(bad("配置项编码在这个类型下重复了")));
            }
        }
    }

    let mut tx = state.app.pool().begin().await.map_err(|e| internal("导入配置失败", e))?;
    let mut result = ImportResult::default();
    let fail = |e| internal("导入配置失败", e);
    for t in &doc.types {
        let (name, remark, code) = (t.name.trim(), t.remark.trim(), t.code.trim());
        let existing: Option<i64> = sqlx::query_scalar("select id from config_types where code = $1").bind(code).fetch_optional(&mut *tx).await.map_err(fail)?;
        let type_id = match existing {
            Some(id) if overwrite => {
                sqlx::query("update config_types set name=$2, remark=$3, status=$4, updated_at=now(), updated_by=$5 where id=$1")
                    .bind(id).bind(name).bind(remark).bind(&t.status).bind(&admin).execute(&mut *tx).await.map_err(fail)?;
                result.updated_types += 1;
                id
            }
            Some(id) => {
                result.skipped_types += 1;
                id
            }
            None => {
                let id: i64 = sqlx::query_scalar("insert into config_types (name, code, remark, status, updated_by) values ($1,$2,$3,$4,$5) returning id")
                    .bind(name).bind(code).bind(remark).bind(&t.status).bind(&admin).fetch_one(&mut *tx).await.map_err(fail)?;
                result.created_types += 1;
                id
            }
        };
        for item in &t.items {
            let (iname, icode, value, label, group, remark) = (item.name.trim(), item.code.trim(), item.value.trim(), item.label.trim(), item.group_name.trim(), item.remark.trim());
            let found: Option<i64> = sqlx::query_scalar("select id from config_items where type_id = $1 and code = $2").bind(type_id).bind(icode).fetch_optional(&mut *tx).await.map_err(fail)?;
            match found {
                Some(id) if overwrite => {
                    sqlx::query("update config_items set name=$2, value=$3, label=$4, group_name=$5, sort=$6, remark=$7, status=$8, updated_at=now(), updated_by=$9 where id=$1")
                        .bind(id).bind(iname).bind(value).bind(label).bind(group).bind(item.sort).bind(remark).bind(&item.status).bind(&admin).execute(&mut *tx).await.map_err(fail)?;
                    result.updated_items += 1;
                }
                Some(_) => result.skipped_items += 1,
                None => {
                    sqlx::query("insert into config_items (type_id, name, code, value, label, group_name, sort, remark, status, updated_by) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
                        .bind(type_id).bind(iname).bind(icode).bind(value).bind(label).bind(group).bind(item.sort).bind(remark).bind(&item.status).bind(&admin).execute(&mut *tx).await.map_err(fail)?;
                    result.created_items += 1;
                }
            }
        }
    }
    tx.commit().await.map_err(fail)?;
    push_params(&state).await;
    Ok(Json(result))
}

/// 执行节点用的运行参数：读 `device_retry`、`stuck_detect` 类型下的配置项。
/// 类型或配置项停用、没有、不是整数时，该项用默认值；最后统一收进合法范围（重试不超过 3 次）。
pub(crate) async fn runtime_params(pool: &sqlx::PgPool) -> protocol::SetParams {
    let default = protocol::SetParams::default();
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "select t.code, i.code, i.value from config_items i join config_types t on t.id = i.type_id
         where t.code in ('device_retry', 'stuck_detect') and t.status = 'normal' and i.status = 'normal'",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let value = |type_code: &str, item_code: &str, fallback: u32| {
        rows.iter()
            .find(|(t, i, _)| t == type_code && i == item_code)
            .and_then(|(_, _, v)| v.trim().parse::<u32>().ok())
            .unwrap_or(fallback)
    };
    protocol::SetParams::clamped(
        value("device_retry", "max_attempts", default.device_retry),
        value("stuck_detect", "stuck_n", default.stuck_n),
        value("stuck_detect", "keep", default.stuck_keep),
    )
}

/// 配置变了：把最新的运行参数发给所有在线节点（离线的节点下次上线时会收到）。
async fn push_params(state: &ApiState) {
    let params = runtime_params(state.app.pool()).await;
    state.app.broadcast(protocol::ServerMsg::SetParams(params)).await;
}

/// 已知配置的取值范围：写入时就拒绝不合理的值，省得写进去才发现没生效。
/// 其余自定义的配置项不限制取值。
fn check_known_value(type_code: &str, item_code: &str, value: &str) -> Result<(), ApiError> {
    use protocol::SetParams as P;
    let (label, min, max) = match (type_code, item_code) {
        ("role_limit", "executor" | "judge" | "case_writer" | "planner" | "ui_review") => ("岗位角色上限", 1, 20),
        ("device_retry", "max_attempts") => ("设备命令重试次数", *P::RETRY_RANGE.start(), *P::RETRY_RANGE.end()),
        ("stuck_detect", "stuck_n") => ("卡住判定次数", *P::STUCK_N_RANGE.start(), *P::STUCK_N_RANGE.end()),
        ("stuck_detect", "keep") => ("卡住检测保留记录数", *P::STUCK_N_RANGE.start(), P::KEEP_MAX),
        ("flow_template", _) => return check_flow_template(value),
        _ => return Ok(()),
    };
    match value.trim().parse::<u32>() {
        Ok(n) if (min..=max).contains(&n) => Ok(()),
        _ => Err(bad(format!("{label}要是 {min} 到 {max} 的整数"))),
    }
}

/// 流程模板的值：JSON 对象，岗位（executor / judge / case_writer / planner / ui_review）对应角色名称，
/// 可以是一个名字，也可以是名字列表（一个岗位可以配多个角色，最多几个由「岗位角色上限」决定，页面上勾选时限制）。
/// 新建任务时选这个流程，就按名称去节点上找对应岗位的角色。至少要有一个岗位。
fn check_flow_template(value: &str) -> Result<(), ApiError> {
    const SLOTS: [&str; 5] = ["executor", "judge", "case_writer", "planner", "ui_review"];
    let invalid = || bad("流程模板的值要是 JSON，例如 {\"case_writer\":[\"角色名\"],\"judge\":[\"角色名1\",\"角色名2\"]}，岗位只能是 executor、judge、case_writer、planner、ui_review，每个岗位 1 到 20 个角色名，每个不超过 60 个字、不能重复");
    let name_ok = |n: &serde_json::Value| n.as_str().is_some_and(|n| !n.trim().is_empty() && n.chars().count() <= 60);
    let parsed: serde_json::Value = serde_json::from_str(value).map_err(|_| invalid())?;
    let object = parsed.as_object().filter(|o| !o.is_empty()).ok_or_else(invalid)?;
    for (slot, names) in object {
        let ok = SLOTS.contains(&slot.as_str())
            && match names {
                serde_json::Value::Array(list) => {
                    let mut seen = std::collections::HashSet::new();
                    (1..=20).contains(&list.len()) && list.iter().all(&name_ok) && list.iter().all(|n| seen.insert(n.as_str().unwrap_or_default().trim().to_string()))
                }
                one => name_ok(one),
            };
        if !ok {
            return Err(invalid());
        }
    }
    Ok(())
}
