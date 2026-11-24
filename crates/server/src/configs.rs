//! 参数配置（只读）：一级配置类型、二级配置项。所有人能看，写入接口在 P2 做（只给管理员）。

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::api::ApiState;
use crate::internal;

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
pub(crate) async fn api_list_types(State(state): State<ApiState>, Query(query): Query<ListQuery>) -> Result<impl IntoResponse, ApiError> {
    let (page, size) = paging(&query);
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
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(Page { total, page, page_size: size, rows })))
}

/// 二级详情：一个配置类型和它下面的配置项（按排序、编号），分页。
pub(crate) async fn api_type_detail(State(state): State<ApiState>, Path(code): Path<String>, Query(query): Query<ListQuery>) -> Result<impl IntoResponse, ApiError> {
    let (page, size) = paging(&query);
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
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(TypeDetail { config_type, items: Page { total, page, page_size: size, rows: items } })))
}
