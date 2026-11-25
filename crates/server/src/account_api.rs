//! 账号管理接口（参数配置 → 账号）：只给前端看账号的元数据、改账号身份。
//! 规则 2：用户名、密码、密文一律不出现在这里的任何返回值里；账号的导入仍走命令行。

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::accounts::identity_exists;
use crate::api::ApiState;
use crate::configs::require_admin;
use crate::internal;

type ApiError = (StatusCode, String);

#[derive(Serialize)]
pub(crate) struct AccountView {
    id: Uuid,
    project: String,
    env: String,
    nickname: String,
    status: String,
    device_serial: Option<String>,
    /// 身份编码；null = 不区分身份
    identity: Option<String>,
}

pub(crate) async fn api_list_accounts(State(state): State<ApiState>) -> Result<Json<Vec<AccountView>>, ApiError> {
    let rows = sqlx::query(
        "select a.id, p.name as project, a.env, a.nickname, a.status, a.device_serial, a.identity
         from test_accounts a join projects p on p.id = a.project_id
         order by p.name, a.env, a.created_at, a.id",
    )
    .fetch_all(state.app.pool())
    .await
    .map_err(|e| internal("查询账号失败", e))?;
    Ok(Json(
        rows.iter()
            .map(|r| AccountView {
                id: r.get("id"),
                project: r.get("project"),
                env: r.get("env"),
                nickname: r.get("nickname"),
                status: r.get("status"),
                device_serial: r.get("device_serial"),
                identity: r.get("identity"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IdentityInput {
    /// 空或 null = 清除身份
    identity: Option<String>,
}

/// 给账号勾选身份（管理员）。身份必须是参数配置「账号身份」里启用的配置项。
pub(crate) async fn api_set_account_identity(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    body: Result<Json<IdentityInput>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &headers).await?;
    let Json(input) = body.map_err(|_| (StatusCode::BAD_REQUEST, "参数格式无效".to_string()))?;
    let identity = input.identity.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let mut conn = state.app.pool().acquire().await.map_err(|e| internal("设置账号身份失败", e))?;
    if let Some(identity) = &identity {
        if !identity_exists(&mut conn, identity).await.map_err(|e| internal("校验账号身份失败", e))? {
            return Err((StatusCode::BAD_REQUEST, "这个身份不在参数配置「账号身份」里，请先在参数配置里添加".to_string()));
        }
    }
    let done = sqlx::query("update test_accounts set identity = $2 where id = $1")
        .bind(id)
        .bind(&identity)
        .execute(&mut *conn)
        .await
        .map_err(|e| internal("设置账号身份失败", e))?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "没有这个账号".to_string()));
    }
    Ok(StatusCode::NO_CONTENT)
}
