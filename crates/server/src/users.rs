//! 发起人身份：个人令牌。库里只存 SHA-256 摘要，明文令牌只在创建时显示一次。

use sqlx::PgPool;
use uuid::Uuid;

use crate::{new_node_secret, secret_hash};

/// 创建用户，返回明文个人令牌（只此一次，库里没有明文）。
pub async fn create_user(pool: &PgPool, name: &str) -> Result<String, sqlx::Error> {
    let token = format!("u_{}", new_node_secret());
    sqlx::query("insert into users (name, token_hash) values ($1, $2)")
        .bind(name)
        .bind(secret_hash(&token))
        .execute(pool)
        .await?;
    Ok(token)
}

/// 凭个人令牌找用户。令牌按摘要精确匹配，摘要列有唯一索引。
pub(crate) async fn user_by_token<'e>(db: impl sqlx::PgExecutor<'e>, token: &str) -> Result<Option<Uuid>, sqlx::Error> {
    if token.is_empty() {
        return Ok(None);
    }
    sqlx::query_scalar("select id from users where token_hash = $1").bind(secret_hash(token)).fetch_optional(db).await
}

/// 按用户名找用户（本机管理命令用）。
pub async fn user_by_name(pool: &PgPool, name: &str) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("select id from users where name = $1").bind(name).fetch_optional(pool).await
}

/// 节点属主检查结果。
#[derive(Debug, PartialEq)]
pub enum NodeOwner {
    /// 节点不存在（交给创建任务的校验报「没有这个节点」）
    Missing,
    Owned,
    /// 属于别人，或还没有属主
    NotYours,
}

pub async fn check_node_owner(pool: &PgPool, node: Uuid, user: Uuid) -> Result<NodeOwner, sqlx::Error> {
    let row: Option<Option<Uuid>> = sqlx::query_scalar("select owner_user_id from nodes where id = $1").bind(node).fetch_optional(pool).await?;
    Ok(match row {
        None => NodeOwner::Missing,
        Some(Some(owner)) if owner == user => NodeOwner::Owned,
        Some(_) => NodeOwner::NotYours,
    })
}
