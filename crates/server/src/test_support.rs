//! 测试辅助：每个测试一个独立的 PostgreSQL schema，互不污染，也不碰开发库里的真实数据。
//!
//! 需要环境变量 `DATABASE_URL`（会自动读取仓库根目录的 `.env`）。`platform_dev` 没有建库权限，
//! 所以隔离靠 schema，不靠新库。schema 名带时间戳，下次调用时顺手清掉十分钟前遗留的。

use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

use crate::accounts::{AccountKey, AccountPool, PoolSettings};
use crate::{AppState, Config};

const PREFIX: &str = "t";

fn database_url() -> String {
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env"));
    std::env::var("DATABASE_URL").expect("测试需要 DATABASE_URL（见仓库根目录 .env）")
}

/// 标识符（schema 名）拼进 SQL 之前必须先校验：只允许字母、数字、下划线。
/// 清理旧 schema 时名字是从数据库读出来的，不能直接信任。
fn ident(name: &str) -> &str {
    assert!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "不安全的 schema 名：{name}"
    );
    name
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// 新建一个空 schema，返回 (连接串, schema 名)。
pub async fn new_schema() -> (String, String) {
    let url = database_url();
    let admin = PgPool::connect(&url).await.expect("连接测试数据库失败");

    // 清掉十分钟前遗留的测试 schema（测试只跑几秒，不会误伤正在跑的）
    let old: Vec<String> = sqlx::query_scalar("select nspname from pg_namespace where nspname like 't\\_%'")
        .fetch_all(&admin)
        .await
        .unwrap_or_default();
    for name in old {
        let ts = name.split('_').nth(1).and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
        if ts + 600 < now_s() {
            if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                let sql = format!("drop schema if exists \"{}\" cascade", ident(&name));
                let _ = sqlx::query(AssertSqlSafe(sql)).execute(&admin).await;
            }
        }
    }

    let schema = format!("{PREFIX}_{}_{}", now_s(), Uuid::new_v4().simple());
    let sql = format!("create schema \"{}\"", ident(&schema));
    sqlx::query(AssertSqlSafe(sql)).execute(&admin).await.expect("创建测试 schema 失败");
    (url, schema)
}

/// 连到指定 schema（每条连接都把 search_path 指过去），跑迁移，返回服务状态。
pub async fn state_for_schema(url: &str, schema: &str, cfg: Config) -> AppState {
    AppState::init(cfg, pool_for_schema(url, schema).await).await.expect("初始化服务状态失败")
}

async fn pool_for_schema(url: &str, schema: &str) -> PgPool {
    let schema = schema.to_string();
    PgPoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _| {
            let sql = format!("set search_path to \"{}\"", ident(&schema));
            Box::pin(async move {
                sqlx::query(AssertSqlSafe(sql)).execute(&mut *conn).await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .expect("连接测试数据库失败")
}

/// 一步到位：新 schema + 服务状态。
pub async fn test_state(cfg: Config) -> AppState {
    let (url, schema) = new_schema().await;
    state_for_schema(&url, &schema, cfg).await
}

/// 只用调用方提供的假密钥和环境，避免并行测试修改进程环境变量。
pub async fn test_state_with_accounts(cfg: Config, key: Option<AccountKey>, env: &str) -> AppState {
    let (url, schema) = new_schema().await;
    state_for_schema_with_accounts(&url, &schema, cfg, key, env).await
}

pub async fn state_for_schema_with_accounts(url: &str, schema: &str, cfg: Config, key: Option<AccountKey>, env: &str) -> AppState {
    let accounts = key.map(|key| AccountPool { key, settings: PoolSettings { env: env.into(), ..PoolSettings::default() } });
    AppState::init_with_accounts(cfg, pool_for_schema(url, schema).await, accounts).await.expect("初始化服务状态失败")
}
