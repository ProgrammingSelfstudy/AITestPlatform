//! 用例导入与查询的数据库测试（每个测试一个独立 schema）。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use protocol::CaseExpected;
use serde_json::{json, Value};
use server::cases::{import, NewCase};
use server::test_support::test_state;
use server::{app, AppState, Config};
use sqlx::Row;

async fn state() -> AppState {
    test_state(Config { node_token: "t".into(), hello_timeout: Duration::from_secs(1), heartbeat_s: 1, lost_grace: Duration::from_secs(120) }).await
}

fn nc(code: &str, level: &str, step: &str) -> NewCase {
    NewCase {
        code: code.into(),
        title: format!("{code} 标题"),
        level: level.into(),
        platforms: vec!["android".into()],
        preconditions: vec!["已登录 {{account.username}}".into()],
        steps: vec![step.into()],
        expected: vec![CaseExpected { idx: 1, text: "预期一".into() }, CaseExpected { idx: 2, text: "预期二".into() }],
        data: json!({"account": "{{account.username}}", "song": "tst_normal_01"}),
        refs: vec!["PRD §1".into()],
        note: String::new(),
        should_fail: level == "故意失败",
    }
}

fn set() -> Vec<NewCase> {
    vec![nc("TC-10", "复杂", "步骤"), nc("TC-02", "简单流程", "步骤"), nc("TC-09", "故意失败", "步骤"), nc("TC-02-F", "故意失败", "步骤")]
}

async fn version(st: &AppState, code: &str) -> i32 {
    sqlx::query_scalar("select version from test_cases where code = $1").bind(code).fetch_one(st.pool()).await.unwrap()
}

#[tokio::test]
async fn 首次导入全部新增_重复导入全部不变且版本不动() {
    let st = state().await;
    let r1 = import(st.pool(), "p", &set(), &BTreeMap::new()).await.unwrap();
    assert_eq!((r1.inserted.len(), r1.updated.len(), r1.unchanged.len()), (4, 0, 0));
    let r2 = import(st.pool(), "p", &set(), &BTreeMap::new()).await.unwrap();
    assert_eq!((r2.inserted.len(), r2.updated.len(), r2.unchanged.len()), (0, 0, 4), "重复导入必须是幂等的");
    assert_eq!(version(&st, "TC-02").await, 1, "没变就不能加版本：run 要靠版本号复现当时的用例");
}

#[tokio::test]
async fn 内容变了才加版本且只加变了的那一个() {
    let st = state().await;
    import(st.pool(), "p", &set(), &BTreeMap::new()).await.unwrap();
    let mut changed = set();
    changed[1] = nc("TC-02", "简单流程", "改过的步骤");
    let r = import(st.pool(), "p", &changed, &BTreeMap::new()).await.unwrap();
    assert_eq!((r.updated.as_slice(), r.unchanged.len()), (&["TC-02".to_string()][..], 3));
    assert_eq!((version(&st, "TC-02").await, version(&st, "TC-10").await), (2, 1));
}

#[tokio::test]
async fn 负例标记被保存() {
    let st = state().await;
    import(st.pool(), "p", &set(), &BTreeMap::new()).await.unwrap();
    let rows = sqlx::query("select code, should_fail from test_cases order by code").fetch_all(st.pool()).await.unwrap();
    let neg: Vec<String> = rows.iter().filter(|r| r.get::<bool, _>("should_fail")).map(|r| r.get("code")).collect();
    assert_eq!(neg, ["TC-02-F", "TC-09"]);
}

#[tokio::test]
async fn 受阻状态_只改状态不加版本() {
    let st = state().await;
    let blocked: BTreeMap<String, String> = [("TC-02".to_string(), "房主不能下麦".to_string())].into();
    import(st.pool(), "p", &set(), &blocked).await.unwrap();
    let r: (String, Option<String>) = sqlx::query_as("select status, blocked_reason from test_cases where code = 'TC-02'").fetch_one(st.pool()).await.unwrap();
    assert_eq!(r, ("blocked".into(), Some("房主不能下麦".into())));

    let again = import(st.pool(), "p", &set(), &blocked).await.unwrap();
    assert_eq!(again.unchanged.len(), 4, "同样的受阻设置再导入是不变");

    let unblock = import(st.pool(), "p", &set(), &BTreeMap::new()).await.unwrap();
    assert_eq!(unblock.status_changed, ["TC-02"], "解除受阻只算状态变化");
    assert_eq!(version(&st, "TC-02").await, 1, "状态变化不是内容变化，不加版本");
    let s: String = sqlx::query_scalar("select status from test_cases where code = 'TC-02'").fetch_one(st.pool()).await.unwrap();
    assert_eq!(s, "active");
}

#[tokio::test]
async fn 指定受阻的用例不存在要报错且什么都不写() {
    let st = state().await;
    let blocked: BTreeMap<String, String> = [("TC-不存在".to_string(), "x".to_string())].into();
    assert!(import(st.pool(), "p", &set(), &blocked).await.unwrap_err().contains("不在导入的用例里"));
    let n: i64 = sqlx::query_scalar("select count(*) from projects").fetch_one(st.pool()).await.unwrap();
    assert_eq!(n, 0, "校验失败时连项目都不能建出来");
}

#[tokio::test]
async fn 导入全有或全无_中途失败整体回滚() {
    let st = state().await;
    let mut cases = set();
    cases[2].steps = vec![]; // 违反库里的 check（steps 不能为空），模拟中途失败
    let err = import(st.pool(), "p", &cases, &BTreeMap::new()).await.unwrap_err();
    assert!(err.contains("数据库错误"), "{err}");
    let (n, p): (i64, i64) = (
        sqlx::query_scalar("select count(*) from test_cases").fetch_one(st.pool()).await.unwrap(),
        sqlx::query_scalar("select count(*) from projects").fetch_one(st.pool()).await.unwrap(),
    );
    assert_eq!((n, p), (0, 0), "失败后前面已经写入的用例和项目都必须回滚");
}

// ---------------------------------------------------------------- 只读接口

async fn serve(st: AppState) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app(st)).await.unwrap() });
    a
}

async fn get(a: SocketAddr, path: &str) -> (u16, Value) {
    let r = reqwest::get(format!("http://{a}{path}")).await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn 接口_项目列表和用例列表按自然顺序() {
    let st = state().await;
    import(st.pool(), "hafa", &set(), &BTreeMap::new()).await.unwrap();
    let a = serve(st).await;
    let (c, v) = get(a, "/api/projects").await;
    assert_eq!((c, v[0]["name"].as_str(), v[0]["cases"].as_i64()), (200, Some("hafa"), Some(4)));
    let (_, cases) = get(a, "/api/projects/hafa/cases").await;
    let codes: Vec<&str> = cases.as_array().unwrap().iter().map(|c| c["code"].as_str().unwrap()).collect();
    assert_eq!(codes, ["TC-02", "TC-02-F", "TC-09", "TC-10"], "TC-10 要排在 TC-02 后面，不是按字符串排");
    assert_eq!(cases[2]["should_fail"], true);
}

#[tokio::test]
async fn 接口_用例详情带下发形态且只有占位符() {
    let st = state().await;
    import(st.pool(), "hafa", &set(), &BTreeMap::new()).await.unwrap();
    let a = serve(st).await;
    let (c, d) = get(a, "/api/projects/hafa/cases/TC-02").await;
    assert_eq!(c, 200);
    assert_eq!(d["version"], 1);
    assert_eq!(d["spec"]["code"], "TC-02");
    assert_eq!(d["spec"]["expected"][1], json!({"idx": 2, "text": "预期二"}));
    assert_eq!(d["spec"]["data"]["account"], "{{account.username}}", "数据里只有占位符");
    assert_eq!(d["refs"][0], "PRD §1");
}

#[tokio::test]
async fn 接口_不存在返回404() {
    let st = state().await;
    import(st.pool(), "hafa", &set(), &BTreeMap::new()).await.unwrap();
    let a = serve(st).await;
    assert_eq!(get(a, "/api/projects/没有/cases").await.0, 404);
    assert_eq!(get(a, "/api/projects/hafa/cases/TC-999").await.0, 404);
}

#[tokio::test]
async fn 接口不暴露导入能力() {
    let st = state().await;
    let a = serve(st).await;
    let r = reqwest::Client::new().post(format!("http://{a}/api/projects/x/cases/import")).body("{}").send().await.unwrap();
    assert!(r.status().is_client_error(), "没有鉴权之前，导入只能是本机命令行，不能是 HTTP 接口：{}", r.status());
}
