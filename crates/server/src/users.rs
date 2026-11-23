//! 发起人身份：个人令牌。库里只存 SHA-256 摘要，明文令牌只在创建时显示一次。

use sqlx::PgPool;
use uuid::Uuid;

use axum::http::HeaderMap;

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

// ---------------------------------------------------------------- 身份：只记名字（靠自觉）

/// 名字：去首尾空白，1 到 40 个字，不能有控制字符。名字不是机密，只用来标明「谁在跑」和「这是谁的节点」。
pub fn clean_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let count = name.chars().count();
    (count >= 1 && count <= 40 && !name.chars().any(char::is_control)).then(|| name.to_string())
}

/// 把 `%E5%B0%8F%E6%98%8E` 这样的百分号编码还原成文字。HTTP 头里不能直接放中文，前端用 encodeURIComponent 编码。
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 浏览器请求里的「我是谁」：来自 `x-user-name`（百分号编码）。没带或格式不对返回 `None`。
pub fn name_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("x-user-name")?.to_str().ok()?;
    clean_name(&percent_decode(raw)?)
}

/// 登记一个名字（已有就返回原来的）。节点启动带 `NODE_USER_NAME` 连上来时调用；用户不能靠别的路径凭空出现。
pub(crate) async fn ensure_user<'e>(db: impl sqlx::PgExecutor<'e>, name: &str) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar("insert into users (name) values ($1) on conflict (name) do update set name = excluded.name returning id")
        .bind(name)
        .fetch_one(db)
        .await
}

/// 浏览器请求的发起人：名字对得上某个已登记用户就是他，否则 `None`（没填、没登记）。
/// **不是认证**：名字人人可填，这里只是「谁在跑」的标识，靠大家自觉；用别人的额度没有技术上的拦截。
pub async fn browser_user(pool: &PgPool, headers: &HeaderMap) -> Result<Option<Uuid>, sqlx::Error> {
    match name_from_headers(headers) {
        Some(name) => user_by_name(pool, &name).await,
        None => Ok(None),
    }
}

/// 测试用：登记一个名字（集成测试在 server 之外，访问不到 `pub(crate)` 的 [`ensure_user`]）。
#[doc(hidden)]
pub async fn ensure_user_for_test(pool: &PgPool, name: &str) -> Result<Uuid, sqlx::Error> {
    ensure_user(pool, name).await
}

/// 创建名字的结果。
pub enum Register {
    Created,
    /// 名字已经有人登记（节点启动登记的或别人创建的）
    Taken,
    /// 要认领的节点不存在，或已经有主人了（不能抢别人的节点）
    NodeUnavailable,
}

pub const MIN_PASSWORD_CHARS: usize = 6;
pub const MAX_PASSWORD_CHARS: usize = 128;

/// 页面上创建名字：名字合格、密码长度合格，名字没被占才登记，密码只存 argon2 哈希。
/// `claim_node`：顺便认领一个还没有主人的节点（同一个事务：认领不成，名字也不创建）。
pub async fn register_with_password(pool: &PgPool, name: &str, password: &str, claim_node: Option<Uuid>) -> anyhow::Result<Register> {
    use argon2::{password_hash::PasswordHasher, Argon2};
    let password = password.to_string();
    // argon2 吃 CPU，放到阻塞线程里算，别卡住异步运行时
    let hash = tokio::task::spawn_blocking(move || Argon2::default().hash_password(password.as_bytes()).map(|h| h.to_string()))
        .await?
        .map_err(|e| anyhow::anyhow!("计算密码哈希失败：{e}"))?;
    let mut tx = pool.begin().await?;
    let user: Option<Uuid> = sqlx::query_scalar("insert into users (name, password_hash) values ($1, $2) on conflict (name) do nothing returning id")
        .bind(name)
        .bind(hash)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(user) = user else { return Ok(Register::Taken) };
    if let Some(node) = claim_node {
        // 只认领「存在且没有主人」的节点；条件写在 update 里，两个人同时认领也只有一个成功
        let claimed = sqlx::query("update nodes set owner_user_id = $1 where id = $2 and owner_user_id is null")
            .bind(user)
            .bind(node)
            .execute(&mut *tx)
            .await?;
        if claimed.rows_affected() != 1 {
            return Ok(Register::NodeUnavailable); // 事务随之回滚，名字不会被创建
        }
    }
    tx.commit().await?;
    Ok(Register::Created)
}

pub async fn list_names(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("select name from users order by name").fetch_all(pool).await
}

pub const NEED_NAME_MESSAGE: &str = "先在页面顶部填「我是谁」（你的名字，和你启动节点时设的 NODE_USER_NAME 一致）";

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn 名字规则_去空白_长度_控制字符() {
        assert_eq!(clean_name("  小明 ").as_deref(), Some("小明"));
        for bad in ["", "   ", "带\n换行"] {
            assert!(clean_name(bad).is_none(), "{bad:?}");
        }
        assert!(clean_name(&"长".repeat(41)).is_none());
        assert!(clean_name(&"长".repeat(40)).is_some());
    }

    #[test]
    fn 请求头里的中文名字靠百分号编码传() {
        let mut h = HeaderMap::new();
        h.insert("x-user-name", "%E5%B0%8F%E6%98%8E".parse().unwrap());
        assert_eq!(name_from_headers(&h).as_deref(), Some("小明"));
        h.insert("x-user-name", "alice".parse().unwrap());
        assert_eq!(name_from_headers(&h).as_deref(), Some("alice"));
        for bad in ["%E5%B0", "%ZZ", "%FF%FE", "%0A"] {
            h.insert("x-user-name", bad.parse().unwrap());
            assert!(name_from_headers(&h).is_none(), "{bad}");
        }
        assert!(name_from_headers(&HeaderMap::new()).is_none());
    }
}
