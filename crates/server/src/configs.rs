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

async fn require_admin(state: &ApiState, headers: &HeaderMap) -> Result<String, ApiError> {
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
        Ok(item) => Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": item })))),
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
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn api_delete_item(State(state): State<ApiState>, headers: HeaderMap, Path((code, item)): Path<(String, i64)>) -> Result<StatusCode, ApiError> {
    require_admin(&state, &headers).await?;
    let id = type_id(&state, &code).await?;
    let done = sqlx::query("delete from config_items where id = $2 and type_id = $1")
        .bind(id).bind(item)
        .execute(state.app.pool())
        .await
        .map_err(|e| internal("删除配置项失败", e))?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "没有这个配置项".to_string()));
    }
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
