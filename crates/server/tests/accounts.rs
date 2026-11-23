//! 账号池、派发与凭证接口的数据库测试。各自使用独立 schema 和编造的凭证。

use std::convert::Infallible;
use std::io::{self, Write};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::ServiceExt;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use protocol::{
    BlockedReason, Caps, CaseExpected, DeviceInfo, DeviceStatus, Envelope, ExecutorDone, FinalResult, Hello,
    JudgeDone, JudgeStatus, ModelSource, ModelSourceCap, NodeMsg, RoleModel, RunBlocked, ServerMsg, Verdict,
};
use serde_json::{Value, json};
use server::accounts::{self, AccountKey, NewAccount, import_accounts};
use server::cases::{self, NewCase};
use server::runs::{self, Claimed, NewTask};
use server::test_support::{new_schema, state_for_schema, test_state_with_accounts};
use server::{AppState, Config, app};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio_tungstenite::tungstenite::Message;
use tracing::instrument::WithSubscriber;
use uuid::Uuid;

const TOKEN: &str = "synthetic-node-token";
const USER: &str = "synthetic-account@hafa.invalid";
const PASSWORD: &str = "synthetic-account-pass!?";
const PROJECT: &str = "测试项目";
const GRACE: Duration = Duration::from_secs(120);

fn key() -> AccountKey {
    AccountKey::from_hex(&"ab".repeat(32)).unwrap()
}

fn account(suffix: &str) -> NewAccount {
    NewAccount {
        username: format!("{USER}{suffix}"),
        password: PASSWORD.into(),
        nickname: "合成样例".into(),
        tags: vec!["voice_room".into()],
        device_serial: None,
    }
}

fn device_serial(index: usize) -> String {
    format!("假设备{index}")
}

fn bound_account(suffix: &str, device: usize) -> NewAccount {
    NewAccount {
        device_serial: Some(device_serial(device)),
        ..account(suffix)
    }
}

fn server_config() -> Config {
    Config {
        node_token: TOKEN.into(),
        hello_timeout: Duration::from_secs(2),
        heartbeat_s: 5,
        lost_grace: GRACE,
    }
}

fn caps() -> Caps {
    Caps {
        node_version: "测试".into(),
        os: "测试系统".into(),
        model_sources: vec![ModelSourceCap {
            source: ModelSource::CodexExec,
            vision: false,
        }],
    }
}

async fn seed_node(pool: &PgPool, devices: usize) -> (Uuid, Vec<Uuid>) {
    let node = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, $2, true)")
        .bind(node)
        .bind(serde_json::to_value(caps()).unwrap())
        .execute(pool)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for i in 0..devices {
        ids.push(sqlx::query_scalar(
            "insert into devices (node_id, serial, platform, status) values ($1, $2, 'android', 'idle') returning id",
        ).bind(node).bind(device_serial(i)).fetch_one(pool).await.unwrap());
    }
    (node, ids)
}

async fn seed_cases(pool: &PgPool, project: &str) {
    let case = |code: &str, step: &str| NewCase {
        code: code.into(),
        title: "合成用例".into(),
        level: String::new(),
        platforms: vec![],
        preconditions: vec![],
        steps: vec![step.into()],
        expected: vec![CaseExpected {
            idx: 1,
            text: "预期".into(),
        }],
        data: json!({}),
        refs: vec![],
        note: String::new(),
        should_fail: false,
    };
    cases::import(
        pool,
        project,
        &[case("A", "使用 {{account.username}} 登录"), case("B", "打开首页")],
        &Default::default(),
    )
    .await
    .unwrap();
}

async fn task(pool: &PgPool, project: &str, node_id: Uuid, codes: &[&str]) -> Vec<Uuid> {
    let role = RoleModel {
        source: ModelSource::CodexExec,
        model: None,
        effort: None,
    };
    runs::create_task(
        pool,
        &NewTask {
            project: project.into(),
            node_id,
            case_codes: codes.iter().map(|code| (*code).into()).collect(),
            executor: role.clone(),
            judge: role,
        },
    )
    .await
    .unwrap()
    .run_ids
}

struct World {
    st: AppState,
    node: Uuid,
    devices: Vec<Uuid>,
}

impl World {
    async fn new(devices: usize, key: Option<AccountKey>, env: &str) -> Self {
        let st = test_state_with_accounts(server_config(), key, env).await;
        let (node, devices) = seed_node(st.pool(), devices).await;
        seed_cases(st.pool(), PROJECT).await;
        Self { st, node, devices }
    }

    fn pool(&self) -> &PgPool {
        self.st.pool()
    }

    async fn queue(&self, codes: &[&str]) -> Vec<Uuid> {
        task(self.pool(), PROJECT, self.node, codes).await
    }

    async fn claim(&self) -> Option<Claimed> {
        self.st.claim_next(self.node).await.unwrap()
    }

    async fn with_lease() -> (Self, Claimed) {
        let w = Self::new(1, Some(key()), "debug").await;
        w.import("debug", &[account("")]).await;
        w.queue(&["A"]).await;
        let claimed = w.claim().await.unwrap();
        (w, claimed)
    }

    async fn import(&self, env: &str, batch: &[NewAccount]) -> usize {
        import_accounts(self.pool(), &key(), PROJECT, env, batch)
            .await
            .unwrap()
    }

    async fn assert_queued_and_unassigned(&self, run: Uuid) {
        let row: (String, Option<Uuid>, Option<Uuid>) =
            sqlx::query_as("select status, device_id, lease_id from runs where id = $1")
                .bind(run)
                .fetch_one(self.pool())
                .await
                .unwrap();
        assert_eq!(row, ("queued".into(), None, None));
    }

    async fn device_status(&self, index: usize) -> String {
        sqlx::query_scalar("select status from devices where id = $1")
            .bind(self.devices[index])
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    async fn set_device_status(&self, index: usize, status: &str) {
        sqlx::query("update devices set status = $2 where id = $1")
            .bind(self.devices[index])
            .bind(status)
            .execute(self.pool())
            .await
            .unwrap();
    }

    async fn leased_account(&self, lease: Uuid) -> (Uuid, Option<String>) {
        sqlx::query_as(
            "select a.id, a.device_serial from account_leases l
             join test_accounts a on a.id = l.account_id where l.id = $1",
        )
        .bind(lease)
        .fetch_one(self.pool())
        .await
        .unwrap()
    }

    async fn disconnect(&self) {
        let mut tx = self.pool().begin().await.unwrap();
        sqlx::query("update devices set status = 'offline' where node_id = $1")
            .bind(self.node)
            .execute(&mut *tx)
            .await
            .unwrap();
        runs::node_lost(&mut tx, self.node).await.unwrap();
        tx.commit().await.unwrap();
    }

    async fn assert_released(&self, lease: Uuid, dirty: bool) {
        let (released, actual_dirty, status, cleanup): (bool, bool, String, bool) = sqlx::query_as(
            "select l.released_at is not null, l.dirty, a.status, a.needs_cleanup from account_leases l
             join test_accounts a on a.id = l.account_id where l.id = $1",
        )
        .bind(lease)
        .fetch_one(self.pool())
        .await
        .unwrap();
        assert!(released);
        assert_eq!(
            (actual_dirty, status.as_str(), cleanup),
            (dirty, if dirty { "cooling" } else { "idle" }, dirty)
        );
    }
}

fn import_from_cli(yaml: &str, device: Option<&str>, database_url: &str) -> Output {
    let dir = std::env::temp_dir().join(format!("platform-account-import-{}", Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("合成导入.yaml");
    std::fs::write(&file, yaml).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_server"));
    command
        .current_dir(&dir)
        .env("DATABASE_URL", database_url)
        .env("HAFA_ACCOUNTS_KEY", "ab".repeat(32))
        // sqlx 在迁移抢咨询锁超过 1 秒时会打慢语句警告：多个测试并行迁移同一个库时必然出现，和被测行为无关
        .env("RUST_LOG", "warn,sqlx::query=error")
        .args(["import-accounts", "--file"])
        .arg(&file)
        .args(["--project", PROJECT]);
    if let Some(device) = device {
        command.args(["--device", device]);
    }
    let output = command.output();
    std::fs::remove_file(file).unwrap();
    std::fs::remove_dir(dir).unwrap();
    output.unwrap()
}

#[test]
fn 导入命令的设备字段错误不回显输入() {
    let yaml = format!(
        "accounts:\n  - username: {USER}\n    password: {PASSWORD}\n    nickname: 合成样例\n    device: [{PASSWORD}]"
    );
    let output = import_from_cli(&yaml, None, "postgres://127.0.0.1/unused_test_db");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("账号 YAML 格式错误"));
    for value in [USER, PASSWORD, "合成样例", "合成导入.yaml"] {
        assert!(!stderr.contains(value));
    }
}

#[tokio::test]
async fn 导入设备字段覆盖命令行_重导更新绑定且只输出数量() {
    let (url, schema) = new_schema().await;
    let st = state_for_schema(&url, &schema, server_config()).await;
    // 子进程也只在这个测试 schema 内运行迁移和导入。
    let url = url.split('#').next().unwrap();
    let separator = if url.contains('?') { '&' } else { '?' };
    let url = format!("{url}{separator}options[search_path]={schema}");
    let (first, second, updated) = (device_serial(0), device_serial(1), device_serial(2));
    let yaml = format!(
        "accounts:
  - username: {USER}-0
    password: {PASSWORD}
    nickname: 合成样例0
  - username: {USER}-1
    password: {PASSWORD}
    nickname: 合成样例1
    device: {second}"
    );
    let import = |device: Option<&str>| {
        let output = import_from_cli(&yaml, device, &url);
        assert!(output.status.success(), "导入命令失败（{}）：{}", output.status, String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout == "已导入 2 个账号\n".as_bytes(), "导入命令的输出不对：{:?}", String::from_utf8_lossy(&output.stdout));
        assert!(output.stderr.is_empty());
    };
    let read_bindings = || {
        sqlx::query_as::<_, (Uuid, Option<String>)>(
            "select id, device_serial from test_accounts order by nickname",
        )
        .fetch_all(st.pool())
    };

    import(Some(first.as_str()));
    let bindings = read_bindings().await.unwrap();
    assert_eq!(bindings.len(), 2);
    let (first_id, second_id) = (bindings[0].0, bindings[1].0);
    assert_eq!(
        bindings,
        [(first_id, Some(first)), (second_id, Some(second.clone()))]
    );

    import(Some(updated.as_str()));
    assert_eq!(
        read_bindings().await.unwrap(),
        [(first_id, Some(updated)), (second_id, Some(second.clone()))]
    );

    import(None);
    assert_eq!(
        read_bindings().await.unwrap(),
        [(first_id, None), (second_id, Some(second))]
    );
}

#[tokio::test]
async fn 导入去重更新密文和昵称且库中没有明文() {
    let st = test_state_with_accounts(server_config(), Some(key()), "debug").await;
    let pool = st.pool();
    assert_eq!(
        import_accounts(pool, &key(), PROJECT, "debug", &[account(""), account("")])
            .await
            .unwrap(),
        1
    );
    let (id, old_password): (Uuid, Vec<u8>) = sqlx::query_as("select id, password_enc from test_accounts")
        .fetch_one(pool)
        .await
        .unwrap();

    let mut edited = account("");
    edited.nickname = "新昵称".into();
    edited.password.push_str("-updated");
    let new_password = edited.password.clone();
    assert_eq!(
        import_accounts(pool, &key(), PROJECT, "debug", &[edited])
            .await
            .unwrap(),
        1
    );
    let rows = sqlx::query("select a.*, to_jsonb(a)::text as raw from test_accounts a")
        .fetch_all(pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.get::<Uuid, _>("id"), id);
    assert_eq!(row.get::<String, _>("nickname"), "新昵称");
    assert_eq!(row.get::<Vec<String>, _>("tags"), ["voice_room"]);
    assert_eq!(
        row.get::<String, _>("username_hash"),
        hex::encode(Sha256::digest(USER.as_bytes()))
    );
    let username_enc: Vec<u8> = row.get("username_enc");
    let password_enc: Vec<u8> = row.get("password_enc");
    assert!(password_enc != old_password);
    assert!(accounts::decrypt(&key(), id, &username_enc).unwrap() == USER);
    assert!(accounts::decrypt(&key(), id, &password_enc).unwrap() == new_password);
    for secret in [USER, PASSWORD, new_password.as_str()] {
        assert!(!row.get::<String, _>("raw").contains(secret));
        for bytes in [&username_enc, &password_enc] {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|slice| slice == secret.as_bytes())
            );
        }
    }
}

#[tokio::test]
async fn 导入任一条写入失败整批回滚() {
    let st = test_state_with_accounts(server_config(), Some(key()), "debug").await;
    let pool = st.pool();
    sqlx::query("alter table test_accounts add check (nickname <> '拒绝该记录')")
        .execute(pool)
        .await
        .unwrap();
    let mut rejected = account("-2");
    rejected.nickname = "拒绝该记录".into();
    let error = import_accounts(pool, &key(), PROJECT, "debug", &[account("-1"), rejected])
        .await
        .unwrap_err();
    assert!(matches!(error, accounts::ImportError::Database));
    let counts: (i64, i64) =
        sqlx::query_as("select (select count(*) from test_accounts), (select count(*) from projects)")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(counts, (0, 0));
    assert!(!error.to_string().contains(USER) && !error.to_string().contains(PASSWORD));
}

#[tokio::test]
async fn 绑定账号不会分给其他设备且普通任务仍可领取() {
    let w = World::new(1, Some(key()), "debug").await;
    w.import("debug", &[bound_account("", 1)]).await;
    let waiting = w.queue(&["A"]).await[0];
    assert!(w.claim().await.is_none());
    w.assert_queued_and_unassigned(waiting).await;
    assert_eq!(w.device_status(0).await, "idle");

    let ordinary = w.queue(&["B"]).await[0];
    let claimed = w.claim().await.unwrap();
    assert_eq!(claimed.run_id, ordinary);
    assert_eq!(claimed.device_id, w.devices[0]);
    assert!(claimed.lease_id.is_none() && !claimed.needs_cleanup);
    w.assert_queued_and_unassigned(waiting).await;
}

#[tokio::test]
async fn 两台设备并发领取各自的绑定账号() {
    let w = World::new(2, Some(key()), "debug").await;
    w.import("debug", &[bound_account("-1", 1), bound_account("-0", 0)])
        .await;
    w.queue(&["A", "A"]).await;
    let (first, second) = tokio::join!(w.claim(), w.claim());
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_ne!(first.run_id, second.run_id);
    assert_ne!(first.device_id, second.device_id);
    let (first_id, first_binding) = w.leased_account(first.lease_id.unwrap()).await;
    let (second_id, second_binding) = w.leased_account(second.lease_id.unwrap()).await;
    assert_ne!(first_id, second_id);
    assert_eq!(first_binding.as_deref(), Some(first.device_serial.as_str()));
    assert_eq!(second_binding.as_deref(), Some(second.device_serial.as_str()));
    assert_eq!(w.device_status(0).await, "busy");
    assert_eq!(w.device_status(1).await, "busy");
}

#[tokio::test]
async fn 本机绑定账号优先于更早导入的未绑定账号() {
    let w = World::new(1, Some(key()), "debug").await;
    w.import("debug", &[account("-unbound")]).await;
    w.import("debug", &[bound_account("-bound", 0)]).await;
    w.queue(&["A"]).await;
    let claimed = w.claim().await.unwrap();
    let (_, binding) = w.leased_account(claimed.lease_id.unwrap()).await;
    assert_eq!(binding, Some(device_serial(0)));
}

#[tokio::test]
async fn 未绑定账号释放后仍可分给另一台设备() {
    let w = World::new(2, Some(key()), "debug").await;
    w.import("debug", &[account("")]).await;
    let run = w.queue(&["A"]).await[0];
    let first = w.claim().await.unwrap();
    let lease = first.lease_id.unwrap();
    let (id, binding) = w.leased_account(lease).await;
    assert_eq!(first.device_id, w.devices[0]);
    assert!(binding.is_none());

    w.disconnect().await;
    w.assert_released(lease, false).await;
    w.set_device_status(1, "idle").await;
    let next = w.claim().await.unwrap();
    assert_eq!(next.run_id, run);
    assert_eq!(next.device_id, w.devices[1]);
    assert_eq!(w.leased_account(next.lease_id.unwrap()).await, (id, None));
}

#[tokio::test]
async fn 绑定设备离线时保持排队_恢复后跳过不匹配设备() {
    let w = World::new(2, Some(key()), "debug").await;
    w.import("debug", &[bound_account("", 1)]).await;
    w.set_device_status(1, "offline").await;
    let run = w.queue(&["A"]).await[0];
    assert!(w.claim().await.is_none());
    w.assert_queued_and_unassigned(run).await;
    assert_eq!(w.device_status(0).await, "idle");

    w.set_device_status(1, "idle").await;
    let claimed = w.claim().await.unwrap();
    assert_eq!(claimed.run_id, run);
    assert_eq!(claimed.device_id, w.devices[1]);
    assert!(claimed.lease_id.is_some());
    assert_eq!(w.device_status(0).await, "idle");
}

#[tokio::test]
async fn 释放后同一账号仍只能分给绑定设备() {
    let w = World::new(2, Some(key()), "debug").await;
    w.import("debug", &[bound_account("", 0)]).await;
    let run = w.queue(&["A"]).await[0];
    let first = w.claim().await.unwrap();
    let old_lease = first.lease_id.unwrap();
    let (id, _) = w.leased_account(old_lease).await;
    assert_eq!(first.device_id, w.devices[0]);

    w.disconnect().await;
    w.assert_released(old_lease, false).await;
    w.set_device_status(1, "idle").await;
    assert!(w.claim().await.is_none());
    w.assert_queued_and_unassigned(run).await;
    assert_eq!(w.device_status(1).await, "idle");

    w.set_device_status(0, "idle").await;
    let reclaimed = w.claim().await.unwrap();
    assert_eq!(reclaimed.run_id, run);
    assert_eq!(reclaimed.device_id, w.devices[0]);
    let new_lease = reclaimed.lease_id.unwrap();
    assert_ne!(new_lease, old_lease);
    assert_eq!(
        w.leased_account(new_lease).await,
        (id, Some(device_serial(0)))
    );
}

#[tokio::test]
async fn 并发领取同一账号只成功一次且库约束阻止重复租约() {
    for bound in [false, true] {
        let w = World::new(2, Some(key()), "debug").await;
        let row = if bound { bound_account("", 1) } else { account("") };
        w.import("debug", &[row]).await;
        let runs = w.queue(&["A", "A"]).await;
        let (first, second) = tokio::join!(w.claim(), w.claim());
        assert_ne!(first.is_some(), second.is_some());
        let claimed = first.or(second).unwrap();
        let unclaimed = *runs.iter().find(|&&run| run != claimed.run_id).unwrap();
        w.assert_queued_and_unassigned(unclaimed).await;
        if bound {
            assert_eq!(claimed.device_id, w.devices[1]);
        }
        let free = w.devices.iter().position(|&device| device != claimed.device_id).unwrap();
        assert_eq!(w.device_status(free).await, "idle");
        let error = sqlx::query(
            "insert into account_leases (account_id, run_id, device_id, expires_at)
             select account_id, $2, $3, now() + interval '1 minute' from account_leases where id = $1",
        )
        .bind(claimed.lease_id.unwrap())
        .bind(unclaimed)
        .bind(w.devices[free])
        .execute(w.pool())
        .await
        .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().constraint(),
            Some("account_leases_account_lock")
        );
    }
}

#[tokio::test]
async fn 脏租约进入冷却且到期后携带清理标志() {
    let w = World::new(2, Some(key()), "debug").await;
    w.import("debug", &[account("")]).await;
    w.queue(&["A", "A"]).await;
    let first = w.claim().await.unwrap();
    let old = first.lease_id.unwrap();
    let mut conn = w.pool().acquire().await.unwrap();
    accounts::release(&mut conn, old, true).await.unwrap();
    w.assert_released(old, true).await;
    let seconds: f64 = sqlx::query_scalar(
        "select extract(epoch from a.cooldown_until - l.released_at)::float8 from test_accounts a
         join account_leases l on l.account_id = a.id where l.id = $1",
    )
    .bind(old)
    .fetch_one(w.pool())
    .await
    .unwrap();
    let configured = std::env::var("HAFA_ACCOUNTS_COOLDOWN_S")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(120);
    assert_eq!(seconds, f64::from(configured));
    sqlx::query("update test_accounts set cooldown_until = now() + interval '1 minute'")
        .execute(w.pool())
        .await
        .unwrap();
    assert!(w.claim().await.is_none());
    sqlx::query("update test_accounts set cooldown_until = now() - interval '1 minute'")
        .execute(w.pool())
        .await
        .unwrap();
    let next = w.claim().await.unwrap();
    assert!(next.needs_cleanup);
    assert_ne!(next.lease_id, first.lease_id);
    accounts::release(&mut conn, old, false).await.unwrap();
    let status: String = sqlx::query_scalar("select status from test_accounts")
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(status, "leased", "重复释放不能影响新租约");
}

#[tokio::test]
async fn 项目环境隔离且隔离状态不会被重新导入重置() {
    let w = World::new(1, Some(key()), "release").await;
    w.import("debug", &[account("")]).await;
    import_accounts(w.pool(), &key(), "另一个项目", "release", &[account("")])
        .await
        .unwrap();
    w.queue(&["A"]).await;
    assert!(w.claim().await.is_none());

    w.import("release", &[account("")]).await;
    let id: Uuid = sqlx::query_scalar(
        "update test_accounts a set status = 'quarantined' from projects p
         where p.id = a.project_id and p.name = $1 and a.env = 'release' returning a.id",
    )
    .bind(PROJECT)
    .fetch_one(w.pool())
    .await
    .unwrap();
    w.import("release", &[account("")]).await;
    assert!(w.claim().await.is_none());
    assert!(
        sqlx::query("update test_accounts set status = '未知状态' where id = $1")
            .bind(id)
            .execute(w.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("update test_accounts set env = '未知环境' where id = $1")
            .bind(id)
            .execute(w.pool())
            .await
            .is_err()
    );
    sqlx::query("update test_accounts set status = 'idle', needs_cleanup = true where id = $1")
        .bind(id)
        .execute(w.pool())
        .await
        .unwrap();
    let claimed = w.claim().await.unwrap();
    let actual: Uuid = sqlx::query_scalar("select account_id from account_leases where id = $1")
        .bind(claimed.lease_id.unwrap())
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(actual, id);
    assert!(claimed.needs_cleanup);
}

#[tokio::test]
async fn 并发领取受节点与项目配额约束() {
    let w = World::new(3, Some(key()), "debug").await;
    w.import("debug", &[account("-1"), account("-2"), account("-3")])
        .await;
    w.queue(&["A", "A", "A"]).await;
    let (first, second, third) = tokio::join!(w.claim(), w.claim(), w.claim());
    assert_eq!([first, second, third].into_iter().flatten().count(), 2);
    let (other_node, _) = seed_node(w.pool(), 1).await;
    task(w.pool(), PROJECT, other_node, &["A"]).await;
    assert!(
        w.st.claim_next(other_node).await.unwrap().is_none(),
        "同一项目已达上限"
    );

    let other_project = "另一个项目";
    seed_cases(w.pool(), other_project).await;
    import_accounts(w.pool(), &key(), other_project, "debug", &[account("")])
        .await
        .unwrap();
    task(w.pool(), other_project, w.node, &["A"]).await;
    assert!(w.claim().await.is_none(), "同一节点已达上限");
    let next = task(w.pool(), other_project, other_node, &["A"]).await[0];
    assert_eq!(w.st.claim_next(other_node).await.unwrap().unwrap().run_id, next);
}

#[tokio::test]
async fn 心跳只续租派发节点的活跃未过期租约() {
    let (w, c) = World::with_lease().await;
    let lease = c.lease_id.unwrap();
    sqlx::query("update account_leases set heartbeat_at = now() - interval '1 minute', expires_at = now() + interval '10 seconds' WHERE id = $1")
        .bind(lease).execute(w.pool()).await.unwrap();
    let mut conn = w.pool().acquire().await.unwrap();
    accounts::heartbeat(&mut conn, Uuid::new_v4(), accounts::DEFAULT_TTL)
        .await
        .unwrap();
    let heartbeat_recent = || {
        sqlx::query_scalar::<_, bool>(
            "select heartbeat_at > now() - interval '10 seconds' from account_leases where id = $1",
        )
        .bind(lease)
        .fetch_one(w.pool())
    };
    assert!(!heartbeat_recent().await.unwrap());
    accounts::heartbeat(&mut conn, w.node, accounts::DEFAULT_TTL)
        .await
        .unwrap();
    assert!(heartbeat_recent().await.unwrap());
    let renewed: bool = sqlx::query_scalar(
        "select expires_at > now() + interval '1 minute' from account_leases where id = $1",
    )
    .bind(lease)
    .fetch_one(w.pool())
    .await
    .unwrap();
    assert!(renewed);
    sqlx::query("update account_leases set expires_at = now() - interval '1 second'")
        .execute(w.pool())
        .await
        .unwrap();
    accounts::heartbeat(&mut conn, w.node, accounts::DEFAULT_TTL)
        .await
        .unwrap();
    let expired: bool = sqlx::query_scalar("select expires_at <= now() from account_leases where id = $1")
        .bind(lease)
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert!(expired);
}

#[tokio::test]
async fn 过期租约只在终态或掉线超过宽限期后回收() {
    let (w, c) = World::with_lease().await;
    let lease = c.lease_id.unwrap();
    runs::mark_started(w.pool(), w.node, c.run_id).await.unwrap();
    sqlx::query("update account_leases set expires_at = now() - interval '1 second'")
        .execute(w.pool())
        .await
        .unwrap();
    assert_eq!(accounts::expire_leases(w.pool(), GRACE).await.unwrap(), 0);
    w.disconnect().await;
    assert_eq!(accounts::expire_leases(w.pool(), GRACE).await.unwrap(), 0);
    sqlx::query("update runs set node_lost_at = now() - interval '3 minutes'")
        .execute(w.pool())
        .await
        .unwrap();
    assert_eq!(accounts::expire_leases(w.pool(), GRACE).await.unwrap(), 1);
    w.assert_released(lease, true).await;

    let (w, c) = World::with_lease().await;
    // 模拟旧状态留下的已结束 run，验证定时任务实际回收它的租约。
    sqlx::query("update runs set status = 'finished', final_result = 'uncertain'")
        .execute(w.pool())
        .await
        .unwrap();
    assert_eq!(
        accounts::expire_leases(w.pool(), GRACE).await.unwrap(),
        0,
        "尚未到达 TTL"
    );
    sqlx::query("update account_leases set expires_at = now() - interval '1 second'")
        .execute(w.pool())
        .await
        .unwrap();
    let housekeeping = w.st.spawn_housekeeping(Duration::from_millis(10));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let released: bool = sqlx::query_scalar("select released_at is not null from account_leases")
                .fetch_one(w.pool())
                .await
                .unwrap();
            if released {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    housekeeping.abort();
    w.assert_released(c.lease_id.unwrap(), true).await;
}

#[tokio::test]
async fn 没有可用账号或主密钥时保持排队_设备空闲且普通任务可派发() {
    for enabled in [true, false] {
        let w = World::new(1, enabled.then(key), "debug").await;
        if !enabled {
            // 即使库中已有可用账号，未配置主密钥时也不能派发。
            w.import("debug", &[account("")]).await;
        }
        let waiting = w.queue(&["A"]).await[0];
        assert!(w.claim().await.is_none());
        w.assert_queued_and_unassigned(waiting).await;
        let device: String = sqlx::query_scalar("select status from devices")
            .fetch_one(w.pool())
            .await
            .unwrap();
        assert_eq!(device, "idle");
        let ordinary = w.queue(&["B"]).await[0];
        let claimed = w.claim().await.unwrap();
        assert_eq!(claimed.run_id, ordinary);
        assert!(claimed.lease_id.is_none() && !claimed.needs_cleanup);
        w.assert_queued_and_unassigned(waiting).await;
    }
}

#[tokio::test]
async fn 派发消息包含租约和清理标志且不含凭证() {
    let w = World::new(0, Some(key()), "debug").await;
    w.import("debug", &[account("")]).await;
    sqlx::query("update test_accounts set needs_cleanup = true")
        .execute(w.pool())
        .await
        .unwrap();
    let run = w.queue(&["A"]).await[0];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(w.st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/node"))
        .await
        .unwrap();
    let hello = Hello {
        token: TOKEN.into(),
        node_id: Some(w.node),
        node_secret: None,
        user_token: None, user_name: None,
        caps: caps(),
        running_runs: vec![],
        devices: vec![DeviceInfo {
            serial: "假设备".into(),
            platform: "android".into(),
            model: None,
            os_version: None,
            screen_w: None,
            screen_h: None,
            status: DeviceStatus::Idle,
        }],
    };
    let msg = serde_json::to_string(&Envelope::new(NodeMsg::Hello(hello))).unwrap();
    socket.send(Message::Text(msg.into())).await.unwrap();
    let assigned = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(Message::Text(msg))) = socket.next().await {
            assert!(!msg.contains(USER) && !msg.contains(PASSWORD));
            let env: Envelope<ServerMsg> = serde_json::from_str(&msg).unwrap();
            if let ServerMsg::RunAssign(assigned) = env.body {
                return assigned;
            }
        }
        panic!("未收到派发消息");
    })
    .await
    .unwrap();
    let lease: Uuid = sqlx::query_scalar("select lease_id from runs where id = $1")
        .bind(run)
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(assigned.run_id, run);
    assert_eq!(assigned.lease_id, Some(lease));
    assert!(assigned.needs_cleanup);
    server.abort();
}

#[tokio::test]
async fn 判定完成干净释放_受阻和取消按脏释放() {
    let (w, c) = World::with_lease().await;
    runs::mark_started(w.pool(), w.node, c.run_id).await.unwrap();
    runs::executor_done(
        w.pool(),
        w.node,
        &ExecutorDone {
            run_id: c.run_id,
            result: Verdict::Passed,
            reason: "步骤结束".into(),
            cost: None,
        },
    )
    .await
    .unwrap();
    runs::judge_done(
        w.pool(),
        w.node,
        &JudgeDone {
            run_id: c.run_id,
            judge_status: JudgeStatus::NotRun,
            final_result: FinalResult::Uncertain,
            items: vec![],
        },
    )
    .await
    .unwrap();
    w.assert_released(c.lease_id.unwrap(), false).await;

    w.queue(&["A"]).await;
    let c = w.claim().await.unwrap();
    runs::run_blocked(
        w.pool(),
        w.node,
        &RunBlocked {
            run_id: c.run_id,
            reason: BlockedReason::Setup,
            detail: "环境错误".into(),
        },
    )
    .await
    .unwrap();
    w.assert_released(c.lease_id.unwrap(), true).await;

    w.import("debug", &[account("-2")]).await;
    w.queue(&["A"]).await;
    let c = w.claim().await.unwrap();
    w.st.cancel_run(c.run_id).await.unwrap();
    w.assert_released(c.lease_id.unwrap(), true).await;
}

#[tokio::test]
async fn 掉线或重启退回排队释放未用租约_重领使用新租约() {
    for restarting in [false, true] {
        let (w, c) = World::with_lease().await;
        let old = c.lease_id.unwrap();
        if restarting {
            let mut tx = w.pool().begin().await.unwrap();
            runs::all_nodes_lost(&mut tx).await.unwrap();
            tx.commit().await.unwrap();
        } else {
            w.disconnect().await;
        }
        w.assert_queued_and_unassigned(c.run_id).await;
        w.assert_released(old, false).await;
        sqlx::query("update devices set status = 'idle'")
            .execute(w.pool())
            .await
            .unwrap();
        let next = w.claim().await.unwrap();
        assert_eq!(next.run_id, c.run_id);
        assert!(next.lease_id.is_some() && next.lease_id != c.lease_id);
        assert_eq!(
            credential_request(&w.st, Method::GET, old, Some(TOKEN), Some(w.node))
                .await
                .status(),
            StatusCode::GONE
        );
    }
}

#[tokio::test]
async fn 掉线退回排队时已取凭证的租约按脏释放() {
    let (w, c) = World::with_lease().await;
    let lease = c.lease_id.unwrap();
    assert_eq!(
        credential_request(&w.st, Method::GET, lease, Some(TOKEN), Some(w.node))
            .await
            .status(),
        StatusCode::OK
    );
    w.disconnect().await;
    w.assert_queued_and_unassigned(c.run_id).await;
    w.assert_released(lease, true).await;
}

#[tokio::test]
async fn 掉线过宽限期和重连孤儿都释放脏租约() {
    for orphan in [true, false] {
        let (w, c) = World::with_lease().await;
        let lease = c.lease_id.unwrap();
        runs::mark_started(w.pool(), w.node, c.run_id).await.unwrap();
        w.disconnect().await;
        assert_eq!(runs::expire_node_lost(w.pool(), GRACE).await.unwrap(), 0);
        let released: bool = sqlx::query_scalar("select released_at is not null from account_leases")
            .fetch_one(w.pool())
            .await
            .unwrap();
        assert!(!released);
        if orphan {
            let mut tx = w.pool().begin().await.unwrap();
            runs::decide_resume(&mut tx, w.node, &[], GRACE).await.unwrap();
            tx.commit().await.unwrap();
        } else {
            sqlx::query("update runs set node_lost_at = now() - interval '3 minutes'")
                .execute(w.pool())
                .await
                .unwrap();
            assert_eq!(runs::expire_node_lost(w.pool(), GRACE).await.unwrap(), 1);
        }
        w.assert_released(lease, true).await;
    }
}

async fn call<S>(mut router: S, request: Request<Body>) -> Response
where
    S: ServiceExt<Request<Body>, Response = Response, Error = Infallible>,
{
    router.call(request).await.unwrap()
}

#[tokio::test]
async fn 页面任务和运行详情可见设备序列号但不含账号信息() {
    let w = World::new(1, Some(key()), "debug").await;
    let row = bound_account("", 0);
    w.import("debug", std::slice::from_ref(&row)).await;
    w.queue(&["A"]).await;
    let claimed = w.claim().await.unwrap();
    let lease = claimed.lease_id.unwrap();
    let (account, _) = w.leased_account(lease).await;
    for (uri, pointer) in [
        (
            format!("/api/tasks/{}", claimed.task_id),
            "/runs/0/device_serial",
        ),
        (format!("/api/runs/{}", claimed.run_id), "/device_serial"),
    ] {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = call(app(w.st.clone()).into_service::<Body>(), request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        for value in [
            USER,
            PASSWORD,
            &row.nickname,
            &row.tags[0],
            &account.to_string(),
            &lease.to_string(),
        ] {
            assert!(!body.contains(value));
        }
        let fields: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            fields.pointer(pointer).and_then(Value::as_str),
            Some(claimed.device_serial.as_str())
        );
    }
}

async fn credential_request(
    st: &AppState,
    method: Method,
    lease: Uuid,
    token: Option<&str>,
    node: Option<Uuid>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/leases/{lease}/credentials"));
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(node) = node {
        request = request.header("X-Node-Id", node.to_string());
    }
    call(
        app(st.clone()).into_service::<Body>(),
        request.body(Body::empty()).unwrap(),
    )
    .await
}

async fn assert_empty_response(response: Response, status: StatusCode) {
    assert_eq!(response.status(), status);
    assert!(!format!("{:?}", response.headers()).contains(PASSWORD));
    assert!(to_bytes(response.into_body(), 8192).await.unwrap().is_empty());
}

async fn assert_credentials(response: Response, account_id: Uuid) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let headers = format!("{:?}", response.headers());
    assert!(!headers.contains(USER) && !headers.contains(PASSWORD));
    let body = to_bytes(response.into_body(), 8192).await.unwrap();
    let fields: Value = serde_json::from_slice(&body).unwrap();
    assert!(fields == json!({"account_id": account_id, "username": USER, "password": PASSWORD}));
}

#[tokio::test]
async fn 凭证只授权派发节点且并发请求也只能成功一次() {
    let (w, c) = World::with_lease().await;
    let lease = c.lease_id.unwrap();
    for token in [None, Some("synthetic-node-tokeN"), Some("wrong")] {
        assert_empty_response(
            credential_request(&w.st, Method::GET, lease, token, Some(w.node)).await,
            StatusCode::UNAUTHORIZED,
        )
        .await;
    }
    for node in [None, Some(Uuid::new_v4())] {
        assert_empty_response(
            credential_request(&w.st, Method::GET, lease, Some(TOKEN), node).await,
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    assert_empty_response(
        credential_request(&w.st, Method::HEAD, lease, Some(TOKEN), Some(w.node)).await,
        StatusCode::METHOD_NOT_ALLOWED,
    )
    .await;
    let fetched: bool = sqlx::query_scalar("select credentials_fetched_at is not null from account_leases")
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert!(!fetched);

    let request = || credential_request(&w.st, Method::GET, lease, Some(TOKEN), Some(w.node));
    let (a, b) = tokio::join!(request(), request());
    let (success, gone) = if a.status() == StatusCode::OK {
        (a, b)
    } else {
        (b, a)
    };
    let id: Uuid =
        sqlx::query_scalar("select account_id from account_leases where credentials_fetched_at is not null")
            .fetch_one(w.pool())
            .await
            .unwrap();
    assert_credentials(success, id).await;
    assert_empty_response(gone, StatusCode::GONE).await;

    for uri in ["/api/accounts", "/api/leases", "/api/accounts/import"] {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = call(app(w.st.clone()).into_service::<Body>(), request).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn 过期或已释放的租约无法首次领取凭证() {
    for expired in [true, false] {
        let (w, c) = World::with_lease().await;
        if expired {
            sqlx::query("update account_leases set expires_at = now() - interval '1 second'")
                .execute(w.pool())
                .await
                .unwrap();
        } else {
            w.st.cancel_run(c.run_id).await.unwrap();
        }
        let response =
            credential_request(&w.st, Method::GET, c.lease_id.unwrap(), Some(TOKEN), Some(w.node)).await;
        assert_empty_response(response, StatusCode::GONE).await;
        let fetched: bool =
            sqlx::query_scalar("select credentials_fetched_at is not null from account_leases")
                .fetch_one(w.pool())
                .await
                .unwrap();
        assert!(!fetched);
    }
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn 解密失败不消费一次性机会且追踪日志和响应头不含凭证() {
    let (w, c) = World::with_lease().await;
    let id: Uuid = sqlx::query_scalar("select id from test_accounts")
        .fetch_one(w.pool())
        .await
        .unwrap();
    let replace_password = |aad| {
        sqlx::query("update test_accounts set password_enc = $2 where id = $1")
            .bind(id)
            .bind(accounts::encrypt(&key(), aad, PASSWORD).unwrap())
            .execute(w.pool())
    };
    replace_password(Uuid::new_v4()).await.unwrap();

    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    async {
        let request =
            || credential_request(&w.st, Method::GET, c.lease_id.unwrap(), Some(TOKEN), Some(w.node));
        assert_empty_response(request().await, StatusCode::INTERNAL_SERVER_ERROR).await;
        let fetched: bool =
            sqlx::query_scalar("select credentials_fetched_at is not null from account_leases")
                .fetch_one(w.pool())
                .await
                .unwrap();
        assert!(!fetched);
        replace_password(id).await.unwrap();
        assert_credentials(request().await, id).await;
    }
    .with_subscriber(subscriber)
    .await;

    let captured = logs.0.lock().unwrap();
    let captured = String::from_utf8_lossy(&captured);
    assert!(captured.contains("解密账号凭证失败"));
    assert!(!captured.contains(USER) && !captured.contains(PASSWORD) && !captured.contains(TOKEN));
}
