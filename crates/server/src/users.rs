//! 有密码的名字使用浏览器会话。节点登记的无密码名字沿用名字识别。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::{
    Argon2, PasswordHash,
    password_hash::{PasswordHasher, PasswordVerifier},
};
use axum::http::HeaderMap;
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

// ---------------------------------------------------------------- 浏览器身份

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

/// 节点带 `NODE_USER_NAME` 连上来时登记名字，已有记录时沿用原来的身份和密码设置。
pub(crate) async fn ensure_user<'e>(db: impl sqlx::PgExecutor<'e>, name: &str) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar("insert into users (name) values ($1) on conflict (name) do update set name = excluded.name returning id")
        .bind(name)
        .fetch_one(db)
        .await
}

pub enum BrowserUser {
    Unspecified,
    Unregistered(String),
    LoginRequired,
    Identified(Uuid),
}

impl BrowserUser {
    pub(crate) fn require(self) -> Result<Uuid, &'static str> {
        match self {
            Self::Identified(user) => Ok(user),
            Self::LoginRequired => Err(NEED_LOGIN_MESSAGE),
            Self::Unspecified | Self::Unregistered(_) => Err(NEED_NAME_MESSAGE),
        }
    }
}

fn token_hash_from_headers(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all("x-user-token").iter();
    let token = values.next()?.to_str().ok()?;
    (!token.is_empty() && values.next().is_none()).then(|| secret_hash(token))
}

/// 解析浏览器请求的发起人。有密码的名字需要属于这个用户的有效会话，无密码的名字按登记记录识别。
pub async fn browser_user(pool: &PgPool, headers: &HeaderMap) -> Result<BrowserUser, sqlx::Error> {
    let Some(name) = name_from_headers(headers) else { return Ok(BrowserUser::Unspecified) };
    let row: Option<(Uuid, bool)> = sqlx::query_as(
        "select id, password_hash is null or exists (
             select 1 from user_sessions where token_hash = $2 and user_id = users.id and expires_at > now()
         ) from users where name = $1",
    )
    .bind(&name)
    .bind(token_hash_from_headers(headers))
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((user, true)) => BrowserUser::Identified(user),
        Some((_, false)) => BrowserUser::LoginRequired,
        None => BrowserUser::Unregistered(name),
    })
}

pub(crate) async fn start_session(pool: &PgPool, user: Uuid) -> anyhow::Result<String> {
    let mut random = [0; 32];
    getrandom::fill(&mut random).map_err(|_| anyhow::anyhow!("生成会话令牌失败"))?;
    let token = hex::encode(random);
    sqlx::query("insert into user_sessions (token_hash, user_id, expires_at) values ($1, $2, now() + interval '720 hours')")
        .bind(secret_hash(&token))
        .bind(user)
        .execute(pool)
        .await?;
    Ok(token)
}

pub(crate) async fn end_session(pool: &PgPool, headers: &HeaderMap) -> Result<(), sqlx::Error> {
    if let Some(hash) = token_hash_from_headers(headers) {
        sqlx::query("delete from user_sessions where token_hash = $1").bind(hash).execute(pool).await?;
    }
    Ok(())
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

const FAILURE_LIMIT: u8 = 5;
const LOCK_TIME: Duration = Duration::from_secs(5 * 60);

pub(crate) enum PasswordCheck {
    Matched(Uuid),
    Rejected,
    Locked,
}

#[derive(Default)]
struct PasswordFailures {
    count: u8,
    locked_until: Option<Instant>,
}

impl PasswordFailures {
    fn locked(&mut self, now: Instant) -> bool {
        if let Some(until) = self.locked_until {
            if now < until {
                return true;
            }
            *self = Self::default();
        }
        false
    }

    fn record_failure(&mut self, now: Instant) {
        self.count += 1;
        if self.count == FAILURE_LIMIT {
            self.locked_until = Some(now + LOCK_TIME);
        }
    }
}

/// 登录和管理员密码校验共用计数。同一个名字串行验证，其他名字各自处理。
#[derive(Default)]
pub(crate) struct PasswordAuth {
    failures: Mutex<HashMap<String, Arc<tokio::sync::Mutex<PasswordFailures>>>>,
}

impl PasswordAuth {
    pub(crate) async fn check(&self, pool: &PgPool, name: &str, password: String, require_admin: bool) -> anyhow::Result<PasswordCheck> {
        // 管理员名字可能由运维直接入库，计数键用定长摘要。
        let key = secret_hash(name);
        let attempts = Arc::clone(self.failures.lock().unwrap().entry(key.clone()).or_default());
        let mut failures = attempts.lock().await;
        if failures.locked(Instant::now()) {
            return Ok(PasswordCheck::Locked);
        }
        let candidate: Option<(Uuid, String)> = sqlx::query_as(
            "select id, password_hash from users where name = $1 and password_hash is not null and (not $2 or is_admin)",
        )
        .bind(name)
        .bind(require_admin)
        .fetch_optional(pool)
        .await?;
        let user = tokio::task::spawn_blocking(move || match candidate {
            Some((user, hash)) => PasswordHash::new(&hash)
                .is_ok_and(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
                .then_some(user),
            None => {
                // 无可用密码时也做一次哈希，避免从校验耗时直接区分名字和密码错误。
                let _ = Argon2::default().hash_password(password.as_bytes());
                None
            }
        })
        .await?;
        if let Some(user) = user {
            *failures = PasswordFailures::default();
            let mut by_name = self.failures.lock().unwrap();
            // 有排队请求时继续共用同一把锁。
            if Arc::strong_count(&attempts) == 2 {
                by_name.remove(&key);
            }
            Ok(PasswordCheck::Matched(user))
        } else {
            failures.record_failure(Instant::now());
            Ok(PasswordCheck::Rejected)
        }
    }
}

pub async fn list_names(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("select name from users order by name").fetch_all(pool).await
}

pub const NEED_NAME_MESSAGE: &str = "先在页面顶部填「我是谁」（你的名字，和你启动节点时设的 NODE_USER_NAME 一致）";
pub(crate) const NEED_LOGIN_MESSAGE: &str = "这个名字设了密码，请先登录";

#[cfg(test)]
#[test]
fn 密码失败五次后锁五分钟_期间的请求不延长锁定() {
    let now = Instant::now();
    let mut failures = PasswordFailures::default();
    for _ in 0..FAILURE_LIMIT {
        assert!(!failures.locked(now));
        failures.record_failure(now);
    }
    assert!(failures.locked(now + LOCK_TIME - Duration::from_secs(1)));
    assert!(!failures.locked(now + LOCK_TIME));
    assert_eq!(failures.count, 0);
}

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
