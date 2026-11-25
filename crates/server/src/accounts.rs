//! 服务端账号加密、导入和租约。明文只在导入和节点的一次性取凭证流程中使用。

use std::collections::HashSet;
use std::time::Duration;

use aes_gcm::aead::{Aead, Generate, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use protocol::LeaseCredentials;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{Acquire, PgConnection, PgPool};
use thiserror::Error;
use uuid::Uuid;

pub const DEFAULT_TTL: Duration = Duration::from_secs(1800);
const DEFAULT_COOLDOWN_S: u32 = 120;
const DEFAULT_MAX_CONCURRENT: u32 = 2;
const NONCE_LEN: usize = 12;

pub struct AccountKey(Aes256Gcm);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("未设置 HAFA_ACCOUNTS_KEY")]
    Missing,
    #[error("HAFA_ACCOUNTS_KEY 必须是 64 位十六进制字符")]
    Invalid,
}

impl AccountKey {
    pub fn from_hex(value: &str) -> Result<Self, KeyError> {
        let mut key = [0; 32];
        hex::decode_to_slice(value, &mut key).map_err(|_| KeyError::Invalid)?;
        Ok(Self(Aes256Gcm::new(&key.into())))
    }

    pub fn from_env() -> Result<Self, KeyError> {
        match std::env::var("HAFA_ACCOUNTS_KEY") {
            Ok(value) => Self::from_hex(&value),
            Err(std::env::VarError::NotPresent) => Err(KeyError::Missing),
            Err(std::env::VarError::NotUnicode(_)) => Err(KeyError::Invalid),
        }
    }
}

#[derive(Debug, Error)]
#[error("账号凭证加解密失败")]
pub struct CipherError;

/// 格式为 12 字节随机 nonce、密文、16 字节认证标签。AAD 使用账号 UUID 的 16 个字节。
pub fn encrypt(key: &AccountKey, account_id: Uuid, plaintext: &str) -> Result<Vec<u8>, CipherError> {
    let nonce = Nonce::try_generate().map_err(|_| CipherError)?;
    let ciphertext = key
        .0
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.as_bytes(),
                aad: account_id.as_bytes(),
            },
        )
        .map_err(|_| CipherError)?;
    let mut bytes = nonce.to_vec();
    bytes.extend(ciphertext);
    Ok(bytes)
}

pub fn decrypt(key: &AccountKey, account_id: Uuid, bytes: &[u8]) -> Result<String, CipherError> {
    let (nonce, ciphertext) = bytes.split_at_checked(NONCE_LEN).ok_or(CipherError)?;
    let nonce = Nonce::try_from(nonce).map_err(|_| CipherError)?;
    let plain = key
        .0
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad: account_id.as_bytes(),
            },
        )
        .map_err(|_| CipherError)?;
    String::from_utf8(plain).map_err(|_| CipherError)
}

fn valid_env(env: &str) -> bool {
    matches!(env, "debug" | "release")
}

pub struct NewAccount {
    pub username: String,
    pub password: String,
    pub nickname: String,
    pub tags: Vec<String>,
    pub device_serial: Option<String>,
    /// 身份编码（参数配置「账号身份」里的配置项编码）；None = 不区分身份
    pub identity: Option<String>,
}

#[derive(Deserialize)]
struct YamlAccount {
    username: String,
    password: String,
    nickname: String,
    tags: Option<Vec<String>>,
    env: Option<String>,
    device: Option<String>,
    identity: Option<String>,
}

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("账号环境必须是 debug 或 release")]
    Env,
    #[error("账号 YAML 格式错误{0}")]
    Yaml(String),
    #[error("第 {0} 项的账号环境与本批导入的环境不一致")]
    MixedEnv(usize),
    #[error("第 {0} 项的用户名或密码为空")]
    EmptyCredentials(usize),
    #[error("第 {0} 项的身份不在参数配置「账号身份」里，请先在参数配置里添加这个身份")]
    UnknownIdentity(usize),
    #[error("导入账号时写入数据库失败")]
    Database,
    #[error(transparent)]
    Cipher(#[from] CipherError),
}

fn validate_account(index: usize, account: &NewAccount) -> Result<(), ImportError> {
    if account.username.trim().is_empty() || account.password.trim().is_empty() {
        return Err(ImportError::EmptyCredentials(index + 1));
    }
    Ok(())
}

pub fn parse_accounts(yaml: &str, env: &str) -> Result<Vec<NewAccount>, ImportError> {
    #[derive(Deserialize)]
    struct Document {
        accounts: Vec<YamlAccount>,
    }

    if !valid_env(env) {
        return Err(ImportError::Env);
    }
    let doc: Document = serde_yaml_ng::from_str(yaml).map_err(|error| {
        // YAML 库的诊断可能带入标量的值，只记录数字位置。
        let location = error.location().map_or_else(String::new, |at| {
            format!("（第 {} 行，第 {} 列）", at.line(), at.column())
        });
        ImportError::Yaml(location)
    })?;
    doc.accounts
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            if row.env.as_deref().is_some_and(|declared| declared != env) {
                return Err(ImportError::MixedEnv(index + 1));
            }
            let account = NewAccount {
                username: row.username,
                password: row.password,
                nickname: row.nickname,
                tags: row
                    .tags
                    .filter(|tags| !tags.is_empty())
                    .unwrap_or_else(|| vec!["voice_room".into()]),
                device_serial: row.device,
                identity: row.identity.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
            };
            validate_account(index, &account)?;
            Ok(account)
        })
        .collect()
}

/// 返回此批导入去重后的数量。更新时使用库中的 ID 加密，保证 AAD 与实际行一致。
pub async fn import_accounts(
    pool: &PgPool,
    key: &AccountKey,
    project: &str,
    env: &str,
    accounts: &[NewAccount],
) -> Result<usize, ImportError> {
    if !valid_env(env) {
        return Err(ImportError::Env);
    }
    if accounts.is_empty() {
        return Ok(0);
    }
    let db = |_| ImportError::Database;
    let mut tx = pool.begin().await.map_err(db)?;
    // 拿项目行锁，让同项目的并发导入先后读到已提交的账号 ID。
    let project_id: Uuid = sqlx::query_scalar(
        "insert into projects (name) values ($1) on conflict (name) do update set name = excluded.name returning id",
    )
    .bind(project)
    .fetch_one(&mut *tx)
    .await.map_err(db)?;
    let mut hashes = HashSet::new();
    for (index, account) in accounts.iter().enumerate() {
        validate_account(index, account)?;
        if let Some(identity) = &account.identity {
            if !identity_exists(&mut tx, identity).await.map_err(db)? {
                return Err(ImportError::UnknownIdentity(index + 1));
            }
        }
        let hash = hex::encode(Sha256::digest(account.username.as_bytes()));
        let id = sqlx::query_scalar(
            "select id from test_accounts where project_id = $1 and env = $2 and username_hash = $3",
        )
        .bind(project_id)
        .bind(env)
        .bind(&hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .unwrap_or_else(Uuid::new_v4);
        let written = sqlx::query(
            "insert into test_accounts (id, project_id, env, username_hash, username_enc, password_enc, nickname, tags, device_serial, identity)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             on conflict (project_id, env, username_hash) do update set
               username_enc = excluded.username_enc, password_enc = excluded.password_enc, nickname = excluded.nickname,
               device_serial = excluded.device_serial, identity = excluded.identity
             where test_accounts.id = excluded.id",
        )
        .bind(id).bind(project_id).bind(env).bind(&hash)
        .bind(encrypt(key, id, &account.username)?)
        .bind(encrypt(key, id, &account.password)?)
        .bind(&account.nickname).bind(&account.tags).bind(&account.device_serial).bind(&account.identity)
        .execute(&mut *tx).await.map_err(db)?;
        if written.rows_affected() != 1 {
            return Err(ImportError::Database);
        }
        hashes.insert(hash);
    }
    tx.commit().await.map_err(db)?;
    Ok(hashes.len())
}

/// 身份编码是否存在于参数配置「账号身份」里（类型和配置项都要是启用状态）。
pub(crate) async fn identity_exists(conn: &mut PgConnection, code: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "select exists(select 1 from config_items i join config_types t on t.id = i.type_id
         where t.code = 'account_identity' and i.code = $1 and i.status = 'normal' and t.status = 'normal')",
    )
    .bind(code)
    .fetch_one(conn)
    .await
}

pub(crate) struct AccountPool {
    pub(crate) key: AccountKey,
    pub(crate) settings: PoolSettings,
}

pub(crate) struct PoolSettings {
    pub(crate) env: String,
    pub(crate) max_concurrent: u32,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            env: "debug".into(),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
        }
    }
}

impl PoolSettings {
    fn from_env() -> Option<Self> {
        let mut settings = Self::default();
        match std::env::var("HAFA_ACCOUNTS_ENV") {
            Ok(env) if valid_env(&env) => settings.env = env,
            Err(std::env::VarError::NotPresent) => {}
            _ => {
                tracing::warn!("HAFA_ACCOUNTS_ENV 必须是 debug 或 release，需要账号的 run 将保持排队");
                return None;
            }
        }
        if let Some(max) = std::env::var("HAFA_ACCOUNTS_MAX_CONCURRENT")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0)
        {
            settings.max_concurrent = max;
        }
        Some(settings)
    }
}

impl AccountPool {
    pub(crate) fn configured() -> Option<Self> {
        let key = match AccountKey::from_env() {
            Ok(key) => key,
            Err(error) => {
                tracing::warn!("{error}，需要账号的 run 将保持排队");
                return None;
            }
        };
        Some(Self {
            key,
            settings: PoolSettings::from_env()?,
        })
    }
}

#[derive(Debug)]
pub struct Lease {
    pub id: Uuid,
    pub account_id: Uuid,
    pub needs_cleanup: bool,
}

pub async fn acquire(
    conn: &mut PgConnection,
    run: Uuid,
    device: Uuid,
    ttl: Duration,
) -> Result<Option<Lease>, sqlx::Error> {
    let Some(settings) = PoolSettings::from_env() else {
        return Ok(None);
    };
    acquire_with_settings(conn, run, device, ttl, &settings).await
}

pub(crate) async fn acquire_with_settings(
    conn: &mut PgConnection,
    run: Uuid,
    device: Uuid,
    ttl: Duration,
    settings: &PoolSettings,
) -> Result<Option<Lease>, sqlx::Error> {
    let mut tx = conn.begin().await?;
    let scope: Option<(Uuid, Uuid, String, Option<String>)> = sqlx::query_as(
        "select t.project_id, r.node_id, d.serial, r.account_identity from runs r join tasks t on t.id = r.task_id
         join devices d on d.id = $2 and d.node_id = r.node_id
         where r.id = $1 and r.status = 'queued' and r.lease_id is null for update of r",
    )
    .bind(run)
    .bind(device)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((project, node, serial, identity)) = scope else {
        return Ok(None);
    };

    // 当前按节点和项目分别限额。固定加锁顺序，让并发领取的计数和写入保持原子性。
    for (namespace, id) in [(0x4841_0001_i32, node), (0x4841_0002_i32, project)] {
        sqlx::query("select pg_advisory_xact_lock($1, hashtext($2))")
            .bind(namespace)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
    }
    let within_quota: bool = sqlx::query_scalar(
        "select count(*) filter (where r.node_id = $1) < $3 and count(*) filter (where a.project_id = $2) < $3
         from account_leases l join runs r on r.id = l.run_id join test_accounts a on a.id = l.account_id
         where l.released_at is null and (r.node_id = $1 or a.project_id = $2)",
    )
    .bind(node).bind(project).bind(i64::from(settings.max_concurrent)).fetch_one(&mut *tx).await?;
    if !within_quota {
        return Ok(None);
    }

    // 冷却完成只恢复可借用状态，清理标志留给下一个持有者。
    sqlx::query(
        "update test_accounts set status = 'idle', cooldown_until = null where id in (
           select id from test_accounts where project_id = $1 and env = $2 and status = 'cooling'
             and cooldown_until <= clock_timestamp() for update skip locked
         )",
    )
    .bind(project)
    .bind(&settings.env)
    .execute(&mut *tx)
    .await?;
    // 匹配的绑定账号优先，同一优先级仍按入库顺序选择。
    let account: Option<(Uuid, bool)> = sqlx::query_as(
        "select id, needs_cleanup from test_accounts where project_id = $1 and env = $2 and status = 'idle'
           and (device_serial is null or device_serial = $3)
           and ($4::text is null or identity = $4)
           and (cooldown_until is null or cooldown_until <= clock_timestamp())
           and not exists (select 1 from account_leases l where l.account_id = test_accounts.id and l.released_at is null)
         order by device_serial is null, created_at, id for update skip locked limit 1",
    )
    .bind(project).bind(&settings.env).bind(serial).bind(identity).fetch_optional(&mut *tx).await?;
    let Some((account_id, needs_cleanup)) = account else {
        return Ok(None);
    };

    // 同一 run 退回排队后可以重领。用新 ID 覆盖已释放的租约，使旧取凭证地址一直失效。
    let id = sqlx::query_scalar(
        "insert into account_leases (account_id, run_id, device_id, acquired_at, heartbeat_at, expires_at)
         values ($1, $2, $3, statement_timestamp(), statement_timestamp(), statement_timestamp() + make_interval(secs => $4))
         on conflict (run_id) do update set id = excluded.id, account_id = excluded.account_id, device_id = excluded.device_id,
           acquired_at = excluded.acquired_at, heartbeat_at = excluded.heartbeat_at, expires_at = excluded.expires_at,
           released_at = null, dirty = false, credentials_fetched_at = null
         where account_leases.released_at is not null returning id",
    )
    .bind(account_id).bind(run).bind(device).bind(ttl.as_secs_f64()).fetch_optional(&mut *tx).await?;
    let Some(id) = id else { return Ok(None) };
    sqlx::query("update test_accounts set status = 'leased' where id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(Lease {
        id,
        account_id,
        needs_cleanup,
    }))
}

pub async fn release(conn: &mut PgConnection, lease_id: Uuid, dirty: bool) -> Result<(), sqlx::Error> {
    release_inner(conn, lease_id, dirty, false).await
}

pub(crate) async fn release_requeued(conn: &mut PgConnection, lease_id: Uuid) -> Result<(), sqlx::Error> {
    release_inner(conn, lease_id, false, true).await
}

async fn release_inner(
    conn: &mut PgConnection,
    lease_id: Uuid,
    dirty: bool,
    requeued: bool,
) -> Result<(), sqlx::Error> {
    let cooldown = std::env::var("HAFA_ACCOUNTS_COOLDOWN_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_COOLDOWN_S);
    // 领取凭证可能先于服务端收到 run_started，退回排队时也要据此判断是否需要清理。
    sqlx::query(
        "with released as (
           update account_leases set released_at = statement_timestamp(), dirty = $2 or ($3 and credentials_fetched_at is not null)
           where id = $1 and released_at is null returning account_id, dirty, released_at
         )
         update test_accounts a set
           status = case when a.status = 'quarantined' then 'quarantined' when r.dirty then 'cooling' else 'idle' end,
           cooldown_until = case when r.dirty then r.released_at + make_interval(secs => $4) end,
           needs_cleanup = r.dirty or ($3 and a.needs_cleanup)
         from released r where a.id = r.account_id",
    )
    .bind(lease_id).bind(dirty).bind(requeued).bind(f64::from(cooldown)).execute(conn).await?;
    Ok(())
}

/// 节点心跳只续租其当前活跃且还有效的租约。
pub async fn heartbeat(conn: &mut PgConnection, node: Uuid, ttl: Duration) -> Result<(), sqlx::Error> {
    sqlx::query(
        "update account_leases l set heartbeat_at = statement_timestamp(), expires_at = statement_timestamp() + make_interval(secs => $2)
         from runs r where r.lease_id = l.id and r.node_id = $1 and r.status in ('assigned', 'running', 'judging')
           and l.released_at is null and l.expires_at > clock_timestamp()",
    )
    .bind(node).bind(ttl.as_secs_f64()).execute(conn).await?;
    Ok(())
}

/// TTL 到期后也要等执行结束或节点掉线超过宽限期，避免把仍在用的账号转给其它设备。
pub async fn expire_leases(pool: &PgPool, grace: Duration) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let ids = sqlx::query_scalar(
        "select l.id from account_leases l join runs r on r.id = l.run_id
         where l.released_at is null and l.expires_at <= clock_timestamp()
           and (r.status in ('finished', 'blocked', 'cancelled') or
             (r.status = 'node_lost' and r.node_lost_at <= clock_timestamp() - make_interval(secs => $1)))
         for update of l skip locked",
    )
    .bind(grace.as_secs_f64())
    .fetch_all(&mut *tx)
    .await?;
    for &id in &ids {
        release(&mut tx, id, true).await?;
    }
    tx.commit().await?;
    Ok(ids.len())
}

pub(crate) enum CredentialError {
    Forbidden,
    Gone,
    Unavailable,
    Cipher,
    Database,
}

pub(crate) async fn take_credentials(
    pool: &PgPool,
    key: Option<&AccountKey>,
    lease: Uuid,
    node: Uuid,
) -> Result<LeaseCredentials, CredentialError> {
    let db = |_| CredentialError::Database;
    let mut tx = pool.begin().await.map_err(db)?;
    let owner: Uuid = sqlx::query_scalar(
        "select r.node_id from account_leases l join runs r on r.id = l.run_id where l.id = $1 for update of l",
    )
    .bind(lease).fetch_optional(&mut *tx).await.map_err(db)?.ok_or(CredentialError::Gone)?;
    if owner != node {
        return Err(CredentialError::Forbidden);
    }
    // 拿到租约行锁后再检查期限。成功解密后才提交领取时间。
    let (account_id, username_enc, password_enc): (Uuid, Vec<u8>, Vec<u8>) = sqlx::query_as(
        "update account_leases l set credentials_fetched_at = clock_timestamp()
         from runs r, test_accounts a
         where l.id = $1 and r.id = l.run_id and r.lease_id = l.id and r.node_id = $2 and a.id = l.account_id
           and r.status in ('assigned', 'running', 'judging', 'node_lost') and a.status = 'leased'
           and l.released_at is null and l.expires_at > clock_timestamp() and l.credentials_fetched_at is null
         returning a.id, a.username_enc, a.password_enc",
    )
    .bind(lease)
    .bind(node)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or(CredentialError::Gone)?;
    let key = key.ok_or(CredentialError::Unavailable)?;
    let username = decrypt(key, account_id, &username_enc).map_err(|_| CredentialError::Cipher)?;
    let password = decrypt(key, account_id, &password_enc).map_err(|_| CredentialError::Cipher)?;
    tx.commit().await.map_err(db)?;
    Ok(LeaseCredentials {
        account_id,
        username,
        password,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: &str) -> AccountKey {
        AccountKey::from_hex(&byte.repeat(32)).unwrap()
    }

    #[test]
    fn 凭证加密使用随机_nonce_并能往返() {
        let key = key("11");
        let id = Uuid::new_v4();
        let value = "合成凭证\n🙂";
        let first = encrypt(&key, id, value).unwrap();
        let second = encrypt(&key, id, value).unwrap();
        assert_ne!(&first[..NONCE_LEN], &second[..NONCE_LEN]);
        assert!(decrypt(&key, id, &first).unwrap() == value);
        assert!(decrypt(&key, id, &second).unwrap() == value);
    }

    #[test]
    fn 凭证解密拒绝密文篡改换账号或换密钥() {
        let id = Uuid::new_v4();
        let (key, other_key) = (key("11"), key("22"));
        let bytes = encrypt(&key, id, "合成凭证").unwrap();
        assert!(decrypt(&key, Uuid::new_v4(), &bytes).is_err());
        assert!(decrypt(&other_key, id, &bytes).is_err());
        assert!(decrypt(&key, id, &bytes[..NONCE_LEN - 1]).is_err());
        for offset in [0, NONCE_LEN, bytes.len() - 1] {
            let mut tampered = bytes.clone();
            tampered[offset] ^= 1;
            assert!(decrypt(&key, id, &tampered).is_err());
        }
    }

    #[test]
    fn 拒绝无效密钥格式且错误不回显值() {
        for invalid in ["", &"f".repeat(63), &"f".repeat(65), &"g".repeat(64)] {
            let error = AccountKey::from_hex(invalid).err().unwrap();
            assert_eq!(error, KeyError::Invalid);
            assert!(invalid.is_empty() || !error.to_string().contains(invalid));
        }
        assert!(AccountKey::from_hex(&"aB".repeat(32)).is_ok());
        assert!(KeyError::Missing.to_string().contains("HAFA_ACCOUNTS_KEY"));
    }

    #[test]
    fn 解析账号列表和默认标签() {
        let rows = parse_accounts(
            "accounts:\n  - username: synthetic\n    password: '01234'\n    nickname: 合成样例\n    env: release\n",
            "release",
        ).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].password == "01234");
        assert_eq!(rows[0].tags, ["voice_room"]);
    }

    #[test]
    fn 解析账号设备字段且缺省为空() {
        let rows = parse_accounts(
            "accounts:
  - username: synthetic-bound
    password: synthetic-only
    nickname: 合成样例
    device: 假设备1
  - username: synthetic-unbound
    password: synthetic-only
    nickname: 合成样例",
            "debug",
        )
        .unwrap();
        assert_eq!(rows[0].device_serial.as_deref(), Some("假设备1"));
        assert!(rows[1].device_serial.is_none());
    }

    #[test]
    fn 账号_yaml_错误不回显输入值() {
        let user = "hidden-test-account.invalid";
        let pass = "only-for-yaml-test!";
        for yaml in [
            format!("accounts: wrong\n{user}: {pass}"),
            format!(
                "accounts:\n  - username: {user}\n    password: {pass}\n    nickname: 样例\n    tags: {pass}"
            ),
            format!(
                "accounts:\n  - username: {user}\n    password: {pass}\n    nickname: 样例\n    env: {pass}"
            ),
            format!(
                "accounts:\n  - username: {user}\n    password: {pass}\n  - username: ''\n    password: {pass}\n    nickname: 样例"
            ),
        ] {
            let error = parse_accounts(&yaml, "debug").err().unwrap();
            for message in [error.to_string(), format!("{error:?}")] {
                assert!(!message.contains(user) && !message.contains(pass));
            }
        }
    }
}
