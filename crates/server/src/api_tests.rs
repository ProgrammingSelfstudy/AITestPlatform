use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::ServiceExt;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Method, Request as HttpRequest};
use protocol::{ExecutorDone, FinalResult, JudgeDone, JudgeItem, JudgeStatus, Step, Verdict};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;

use super::*;
use crate::cases::{self, NewCase};
use crate::test_support::test_state;
use crate::{Config, Inner, app_with_paths};

const MODELS: &str = r#"{
  "roles": [
    {"id":"executor-codex","slot":"executor","name":"执行员 · Codex","source":"codex_exec",
     "default":{"model":"测试执行模型","effort":"max"},
     "models":[{"model":"测试执行模型","efforts":["high","max"]},{"model":"另一执行模型","efforts":["low"]}]},
    {"id":"judge-claude","slot":"judge","name":"判定员 · Claude","source":"claude_cli",
     "default":{"model":"测试判定模型","effort":null},
     "models":[{"model":"测试判定模型","efforts":[]}]},
    {"id":"judge-agy","slot":"judge","name":"判定员 · Gemini","source":"agy",
     "default":{"model":"测试Gemini模型","effort":null},
     "models":[{"model":"测试Gemini模型","efforts":[]}]}
  ]
}"#;
const TOKEN: &str = "test-node-token";
/// 测试用户的名字「测试用户」，请求头里中文要百分号编码（身份只记名字，不是机密）
const USER_NAME: &str = "%E6%B5%8B%E8%AF%95%E7%94%A8%E6%88%B7";
const REVIEW_TOKEN: &str = "synthetic-review-token-012345";
const CREATE_ROUTE: &str = "/api/tasks";
const BAD_CANCEL_ROUTE: &str = "/api/runs/not-a-uuid/cancel";
const REVIEW_ROUTE: &str = "/api/runs/00000000-0000-0000-0000-000000000000/reviews";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const MP4: &[u8] = b"\0\0\0\x10ftypisom\0\0\0\0";
const WRITE_HEADERS: [(&str, &str); 4] = [
    ("host", "127.0.0.1:5173"),
    ("origin", "http://localhost:5173"),
    ("x-requested-with", "ai-test-platform"),
    ("content-type", "application/json"),
];

fn config() -> Config {
    Config {
        node_token: TOKEN.into(),
        hello_timeout: Duration::from_secs(1),
        heartbeat_s: 1,
        lost_grace: Duration::from_secs(120),
    }
}

fn lazy_state() -> AppState {
    AppState(Arc::new(Inner {
        cfg: config(),
        pool: PgPoolOptions::new()
            .connect_lazy("postgres://127.0.0.1/unused_test_db")
            .unwrap(),
        accounts: None,
        review_auth: reviews::ReviewAuth::new(None),
        conns: tokio::sync::Mutex::new(HashMap::new()),
        roles: Default::default(),
        apps: Default::default(),
    }))
}

fn with_token(mut state: AppState, token: Option<&str>) -> AppState {
    Arc::get_mut(&mut state.0).unwrap().review_auth =
        reviews::ReviewAuth::new(token.map(str::to_owned));
    state
}

fn review_headers(token: &str) -> Vec<(&str, &str)> {
    let mut headers = WRITE_HEADERS.to_vec();
    headers.push(("x-review-token", token));
    headers
}

struct TempPaths {
    root: PathBuf,
    paths: Paths,
}

impl TempPaths {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("platform-api-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let paths = Paths {
            models: root.join("models.json"),
            data: root.join("data"),
        };
        Self { root, paths }
    }

    fn with_config() -> Self {
        let temp = Self::new();
        std::fs::write(&temp.paths.models, MODELS).unwrap();
        temp
    }

    fn app(&self, state: AppState) -> Router {
        app_with_paths(state, self.paths.clone(), false)
    }
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn request(
    app: &Router,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: impl Into<Body>,
) -> (StatusCode, HeaderMap, Bytes) {
    let mut request = HttpRequest::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = call_http(
        app.clone().into_service::<Body>(),
        request.body(body.into()).unwrap(),
    )
    .await;
    let (parts, body) = response.into_parts();
    (
        parts.status,
        parts.headers,
        to_bytes(body, files::MAX_SIZE).await.unwrap(),
    )
}

async fn call_http<S>(mut service: S, request: HttpRequest<Body>) -> Response
where
    S: ServiceExt<HttpRequest<Body>, Response = Response, Error = Infallible>,
{
    service.call(request).await.unwrap()
}

fn text(body: &Bytes) -> &str {
    std::str::from_utf8(body).unwrap()
}

fn json_body(body: &Bytes) -> Value {
    serde_json::from_slice(body).unwrap()
}

fn payload(node: Uuid, codes: &[&str]) -> Value {
    json!({
        "project": "hafa",
        "node_id": node,
        "case_codes": codes,
        "executor": {"model": "测试执行模型", "effort": "max", "source": "claude_cli"},
        "judge": {"model": "测试判定模型", "source": "codex_exec"},
    })
}

async fn browser_post(app: &Router, path: &str, body: Value) -> (StatusCode, HeaderMap, Bytes) {
    request(
        app,
        Method::POST,
        path,
        &{
            let mut headers = review_headers(REVIEW_TOKEN);
            headers.push(("x-user-name", USER_NAME));
            headers
        },
        body.to_string(),
    )
    .await
}

async fn rejected(headers: &[(&str, &str)]) {
    let temp = TempPaths::new();
    let app = temp.app(with_token(lazy_state(), Some(REVIEW_TOKEN)));
    let mut headers = headers.to_vec();
    headers.push(("x-review-token", REVIEW_TOKEN));
    for path in [
        "/api/tasks",
        "/api/runs/00000000-0000-0000-0000-000000000000/cancel",
    ] {
        let (status, _, body) = request(&app, Method::POST, path, &headers, "{}").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.is_empty());
    }
}

#[tokio::test]
async fn 写接口拒绝非回环_host_防_dns_重绑定() {
    for host in [
        "rebind.invalid:5173",
        "127.0.0.1.rebind.invalid",
        "localhost.",
        "127.1",
        "localhost@rebind.invalid",
        "rebind.invalid@localhost",
    ] {
        let mut headers = WRITE_HEADERS;
        headers[0].1 = host;
        rejected(&headers).await;
    }
    rejected(&WRITE_HEADERS[1..]).await;
}

#[tokio::test]
async fn 写接口拒绝非回环或非法_origin() {
    for origin in [
        "https://other.invalid",
        "null",
        "http://localhost@other.invalid",
        "http://localhost/path",
        "http://localhost, http://other.invalid",
    ] {
        let mut headers = WRITE_HEADERS;
        headers[1].1 = origin;
        rejected(&headers).await;
    }
    let mut duplicates = WRITE_HEADERS.to_vec();
    duplicates.push(("origin", "http://other.invalid"));
    rejected(&duplicates).await;
}

#[tokio::test]
async fn 写接口要求专用请求头() {
    rejected(&[WRITE_HEADERS[0], WRITE_HEADERS[1], WRITE_HEADERS[3]]).await;
    let mut headers = WRITE_HEADERS;
    headers[2].1 = "XMLHttpRequest";
    rejected(&headers).await;
}

#[tokio::test]
async fn 写接口要求application_json() {
    rejected(&WRITE_HEADERS[..3]).await;
    for mime in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "application/json-other",
    ] {
        let mut headers = WRITE_HEADERS;
        headers[3].1 = mime;
        rejected(&headers).await;
    }
}

#[tokio::test]
async fn 回环写请求通过且_get_不受同一规则限制() {
    let temp = TempPaths::with_config();
    let app = temp.app(with_token(lazy_state(), Some(REVIEW_TOKEN)));
    // 坏 JSON 被业务处理器拒绝，说明这些合法的写请求已通过中间件。
    for (host, origin) in [
        ("127.0.0.1", None),
        ("LOCALHOST:5173", Some("http://127.0.0.1:8080")),
        ("[::1]:65535", Some("https://[::1]")),
    ] {
        let mut headers = vec![("host", host), WRITE_HEADERS[2], WRITE_HEADERS[3]];
        if let Some(origin) = origin {
            headers.push(("origin", origin));
        }
        headers.push(("x-review-token", REVIEW_TOKEN));
        let (status, _, body) = request(&app, Method::POST, "/api/tasks", &headers, "{").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(text(&body).contains("任务参数格式无效"));
    }
    let (status, _, _) = request(
        &app,
        Method::GET,
        "/api/models",
        &[
            ("host", "external.invalid"),
            ("origin", "http://external.invalid"),
        ],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn 创建和取消缺少或用错口令均返回空体403() {
    let temp = TempPaths::new();
    let app = temp.app(with_token(lazy_state(), Some(REVIEW_TOKEN)));
    let wrong = review_headers("wrong-token");
    for path in [CREATE_ROUTE, BAD_CANCEL_ROUTE] {
        for headers in [&WRITE_HEADERS[..], &wrong[..]] {
            let (status, _, body) = request(&app, Method::POST, path, headers, "{").await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
            assert!(body.is_empty());
        }
    }
}

#[tokio::test]
async fn 未配置或过短口令时创建和取消始终返回空体403() {
    let temp = TempPaths::new();
    for token in [None, Some("short-token")] {
        let app = temp.app(with_token(lazy_state(), token));
        let headers = review_headers(token.unwrap_or(REVIEW_TOKEN));
        for path in [CREATE_ROUTE, BAD_CANCEL_ROUTE] {
            for _ in 0..12 {
                let (status, _, body) = request(&app, Method::POST, path, &headers, "{").await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
                assert!(body.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn 创建取消与审核共用失败计数和锁定且不影响读取() {
    let temp = TempPaths::with_config();
    let app = temp.app(with_token(lazy_state(), Some(REVIEW_TOKEN)));
    let wrong = review_headers("wrong-token");
    let attempts = std::iter::repeat_n(CREATE_ROUTE, 6)
        .chain(std::iter::repeat_n(BAD_CANCEL_ROUTE, 2))
        .chain(std::iter::repeat_n(REVIEW_ROUTE, 3));
    for (index, path) in attempts.enumerate() {
        let (status, _, body) = request(&app, Method::POST, path, &wrong, "{").await;
        let expected = if index == 10 {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(status, expected, "{path}");
        assert!(body.is_empty());
    }
    let correct = review_headers(REVIEW_TOKEN);
    for path in [CREATE_ROUTE, BAD_CANCEL_ROUTE, REVIEW_ROUTE] {
        for headers in [&correct[..], &WRITE_HEADERS[..]] {
            let (status, _, body) = request(&app, Method::POST, path, headers, "{").await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{path}");
            assert!(body.is_empty());
        }
    }
    let (status, _, body) = request(&app, Method::GET, "/api/tasks?limit=-1", &[], "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(text(&body), "limit 必须是非负整数");
    assert_eq!(
        request(&app, Method::GET, "/api/models", &[], "").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn 正确口令下局域网创建和取消保留原有参数错误码() {
    let temp = TempPaths::new();
    let state = with_token(lazy_state(), Some(REVIEW_TOKEN));
    let app = app_with_paths(state, temp.paths.clone(), true);
    let mut headers = review_headers(REVIEW_TOKEN);
    headers[0].1 = "192.168.1.10:5173";
    headers[1].1 = "http://192.168.1.10:5173";

    let (status, _, body) = request(&app, Method::POST, CREATE_ROUTE, &headers, "{").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&body).contains("任务参数格式无效"));
    let (status, _, body) = request(&app, Method::POST, BAD_CANCEL_ROUTE, &headers, "{}").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(text(&body), "没有这个 run");
}

#[tokio::test]
async fn models_缺失时返回500和中文说明() {
    let temp = TempPaths::new();
    let app = temp.app(lazy_state());
    let (status, _, body) = request(&app, Method::GET, "/api/models", &[], "").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(text(&body).contains("模型配置文件不存在"));
}

#[tokio::test]
async fn models_原样返回每次重读并拒绝坏配置() {
    let temp = TempPaths::with_config();
    let app = temp.app(lazy_state());
    let (status, headers, body) = request(&app, Method::GET, "/api/models", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(text(&body), MODELS);
    let changed = MODELS.replace("测试判定模型", "新判定模型");
    std::fs::write(&temp.paths.models, &changed).unwrap();
    let (_, _, body) = request(&app, Method::GET, "/api/models", &[], "").await;
    assert_eq!(text(&body), changed);
    // 坏配置：不是 JSON、缺 roles、没有判定员岗位、重复 id、还没有运行器的来源（deepseek）、默认值不在列表里
    let no_judge = r#"{"roles":[{"id":"e","slot":"executor","name":"x","source":"codex_exec","default":{"model":"m","effort":"high"},"models":[{"model":"m","efforts":["high"]}]}]}"#;
    let dup = MODELS.replace("judge-agy", "judge-claude");
    let deepseek = MODELS.replace("\"source\":\"agy\"", "\"source\":\"deepseek\"");
    let bad_default = MODELS.replace("\"model\":\"测试Gemini模型\",\"effort\":null", "\"model\":\"不存在\",\"effort\":null");
    for broken in ["{", "{}", no_judge, dup.as_str(), deepseek.as_str(), bad_default.as_str()] {
        std::fs::write(&temp.paths.models, broken).unwrap();
        let (status, _, body) = request(&app, Method::GET, "/api/models", &[], "").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(text(&body).contains("模型配置文件格式"));
    }
}

#[tokio::test]
async fn 创建任务拒绝未配置的模型和强度() {
    let temp = TempPaths::with_config();
    let app = temp.app(with_token(lazy_state(), Some(REVIEW_TOKEN)));
    let good = payload(Uuid::new_v4(), &["TC-04"]);
    for (role, field, value, message) in [
        ("executor", "model", "未配置", "执行员模型"),
        ("executor", "effort", "low", "推理强度"),
        ("judge", "model", "未配置", "判定员模型"),
    ] {
        let mut body = good.clone();
        body[role][field] = json!(value);
        let (status, _, bytes) = browser_post(&app, "/api/tasks", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(text(&bytes).contains(message));
    }
}

#[tokio::test]
async fn 创建任务按角色选来源_不写角色用岗位第一个() {
    let temp = TempPaths::with_config();
    let app = temp.app(with_token(lazy_state(), Some(REVIEW_TOKEN)));
    let good = payload(Uuid::new_v4(), &["TC-04"]);
    // 选了 Gemini 角色但模型是 Claude 角色的：该角色下没有这个模型
    let mut body = good.clone();
    body["judge"]["role"] = json!("judge-agy");
    let (status, _, bytes) = browser_post(&app, "/api/tasks", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("判定员模型"));
    // 不存在的角色
    let mut body = good.clone();
    body["judge"]["role"] = json!("judge-nope");
    let (status, _, bytes) = browser_post(&app, "/api/tasks", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("判定员角色"));
    // 没有档位的模型不能带强度
    let mut body = good;
    body["judge"]["role"] = json!("judge-agy");
    body["judge"]["model"] = json!("测试Gemini模型");
    let (status, _, _) = browser_post(&app, "/api/tasks", body).await;
    assert_ne!(status, StatusCode::BAD_REQUEST, "角色和模型匹配时校验应通过（后面的节点属主校验另有结果）");
}

// ---------------------------------------------------------------- 节点角色配置

fn report_role(id: &str, slot: protocol::RoleSlot, source: ModelSource, model: &str, efforts: &[&str], default_effort: Option<&str>, status: protocol::CheckStatus) -> protocol::RoleReport {
    protocol::RoleReport {
        config: RoleConfig {
            id: id.into(),
            slot,
            name: format!("角色-{id}"),
            source,
            default: protocol::RoleDefault { model: model.into(), effort: default_effort.map(str::to_owned) },
            models: vec![protocol::RoleModelOption { model: model.into(), efforts: efforts.iter().map(|e| e.to_string()).collect() }],
        },
        key_configured: false,
        check: protocol::RoleCheck { status, detail: None, checked_at: None },
    }
}

fn user_headers(token: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![("x-user-name", token)]
}

fn set_roles_body(roles: Value) -> String {
    json!({ "roles": roles }).to_string()
}

fn role_json(id: &str, slot: &str, source: &str, model: &str) -> Value {
    json!({"id": id, "slot": slot, "name": id, "source": source, "default": {"model": model, "effort": null}, "models": [{"model": model, "efforts": []}]})
}

#[tokio::test]
async fn 角色接口_查看公开_修改和检测只能是节点主人() {
    let world = World::new().await;
    let path = format!("/api/nodes/{}/roles", world.node);
    // 查看是公开的：不填名字也能看，只是不知道「是不是你的」
    let (status, _, bytes) = request(&world.app, Method::GET, &path, &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json_body(&bytes)["mine"].is_null());
    // 填了名字：是主人 mine=true；别人 mine=false；没登记的名字 mine=false 并说明原因
    sqlx::query("insert into users (name) values ('other-user')").execute(world.state.pool()).await.unwrap();
    let mine = |name: &'static str| {
        let (app, path) = (world.app.clone(), path.clone());
        async move { json_body(&request(&app, Method::GET, &path, &user_headers(name), "").await.2) }
    };
    assert_eq!(mine(USER_NAME).await["mine"], true);
    assert_eq!(mine("other-user").await["mine"], false);
    let unknown = mine("nobody").await;
    assert_eq!(unknown["mine"], false);
    assert!(unknown["name_problem"].as_str().unwrap().contains("还没有登记"), "{unknown}");
    // 不存在的节点 404
    assert_eq!(request(&world.app, Method::GET, &format!("/api/nodes/{}/roles", Uuid::new_v4()), &[], "").await.0, StatusCode::NOT_FOUND);
    // 修改和检测（花额度）：没填名字 / 名字没登记 401；别人的节点 403；没有属主的节点 403
    let write = |name: Option<&'static str>| {
        let mut h = review_headers(REVIEW_TOKEN);
        if let Some(name) = name {
            h.push(("x-user-name", name));
        }
        let (app, path) = (world.app.clone(), format!("{path}/init-template"));
        async move { request(&app, Method::POST, &path, &h, "{}").await.0 }
    };
    assert_eq!(write(None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(write(Some("nobody")).await, StatusCode::UNAUTHORIZED);
    assert_eq!(write(Some("other-user")).await, StatusCode::FORBIDDEN, "别人的节点：能看，不能改");
    sqlx::query("update nodes set owner_user_id = null where id = $1").bind(world.node).execute(world.state.pool()).await.unwrap();
    assert_eq!(write(Some(USER_NAME)).await, StatusCode::FORBIDDEN, "没有属主的节点谁都不能改");
}

#[tokio::test]
async fn 角色接口_离线不返回角色_保存先校验再转发() {
    let world = World::new().await;
    let path = format!("/api/nodes/{}/roles", world.node);
    world.state.set_roles_report_for_test(world.node, protocol::RolesReport { roles: vec![report_role("a", protocol::RoleSlot::Judge, ModelSource::Agy, "m", &[], None, protocol::CheckStatus::Ok)] });
    // 测试里节点没有 WebSocket 连接：离线，不返回上一次的（可能过期的）角色
    let (status, headers, bytes) = request(&world.app, Method::GET, &path, &user_headers(USER_NAME), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(json_body(&bytes), json!({"online": false, "roles": [], "mine": true, "name_problem": null}));
    // 保存：无效配置先被拒（400），不会因为节点离线而掩盖
    let post = |body: String| {
        let mut headers = review_headers(REVIEW_TOKEN);
        headers.push(("x-user-name", USER_NAME));
        let (app, path) = (world.app.clone(), path.clone());
        async move {
            request(&app, Method::POST, &path, &headers, body).await
        }
    };
    let exec = role_json("e", "executor", "codex_exec", "m");
    let judge = role_json("j", "judge", "agy", "m");
    for (roles, why) in [
        (json!([exec]), "缺判定员"),
        (json!([exec, role_json("j", "judge", "deepseek", "m")]), "还没有运行器的来源"),
        (json!([exec, role_json("j", "judge", "codex_exec", "m")]), "来源和岗位不匹配"),
        (json!([exec, judge, role_json("j", "judge", "agy", "m")]), "id 重复"),
    ] {
        let (status, _, bytes) = post(set_roles_body(roles)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}：{}", text(&bytes));
    }
    // 配置合法但节点离线：409，如实告知
    let (status, _, bytes) = post(set_roles_body(json!([exec, judge]))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(text(&bytes).contains("节点离线"));
    // 检测：节点上没有这个角色 404；有但离线 409
    let check = |role: &str| {
        let mut h = review_headers(REVIEW_TOKEN);
        h.push(("x-user-name", USER_NAME));
        let (app, p) = (world.app.clone(), format!("{path}/{role}/check"));
        async move { request(&app, Method::POST, &p, &h, "{}").await }
    };
    assert_eq!(check("不存在").await.0, StatusCode::NOT_FOUND);
    assert_eq!(check("a").await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn 创建任务用节点上报的角色_没检测通过或没配置都拒绝() {
    let world = World::new().await;
    let exec = |status| report_role("exec", protocol::RoleSlot::Executor, ModelSource::CodexExec, "测试执行模型", &["high", "max"], Some("max"), status);
    let judge = |id: &str, source, status| report_role(id, protocol::RoleSlot::Judge, source, "测试判定模型", &[], None, status);
    let post = |body: Value| browser_post(&world.app, "/api/tasks", body);
    // 节点上报了空列表：提示去配置
    world.state.set_roles_report_for_test(world.node, protocol::RolesReport { roles: vec![] });
    let (status, _, bytes) = post(payload(world.node, &["TC-04"])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("还没有配置角色"));
    // 角色存在但没检测通过：拒绝，并说明是哪个角色
    world.state.set_roles_report_for_test(world.node, protocol::RolesReport { roles: vec![exec(protocol::CheckStatus::Ok), judge("judge-agy", ModelSource::Agy, protocol::CheckStatus::Failed)] });
    let mut body = payload(world.node, &["TC-04"]);
    body["judge"]["role"] = json!("judge-agy");
    let (status, _, bytes) = post(body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("还没有检测通过") && text(&bytes).contains("角色-judge-agy"), "{}", text(&bytes));
    // 全部通过：用所选角色的来源建任务（来源由服务端按角色定，不信请求里带的）
    // 节点的命令行能力里要有 agy，否则创建任务本身会拒绝（这是原有的另一道校验）
    sqlx::query("update nodes set caps = $2 where id = $1").bind(world.node)
        .bind(json!({"model_sources": [{"source": "claude_cli"}, {"source": "codex_exec"}, {"source": "agy"}]}))
        .execute(world.state.pool()).await.unwrap();
    world.state.set_roles_report_for_test(world.node, protocol::RolesReport { roles: vec![exec(protocol::CheckStatus::Ok), judge("judge-agy", ModelSource::Agy, protocol::CheckStatus::Ok)] });
    let mut body = payload(world.node, &["TC-04"]);
    body["judge"]["role"] = json!("judge-agy");
    let (status, _, bytes) = post(body).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    let task = json_body(&bytes)["task_id"].clone();
    let (_, _, detail) = request(&world.app, Method::GET, &format!("/api/tasks/{}", task.as_str().unwrap()), &[], "").await;
    assert_eq!(json_body(&detail)["judge"]["source"], "agy");
    // 别人拿自己的令牌往这个节点发任务：403，不泄露角色情况
    sqlx::query("insert into users (name) values ('another-person')").execute(world.state.pool()).await.unwrap();
    let mut headers = review_headers(REVIEW_TOKEN);
    headers.push(("x-user-name", "another-person"));
    let (status, _, _) = request(&world.app, Method::POST, "/api/tasks", &headers, payload(world.node, &["TC-04"]).to_string()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------- 模块与用例生成请求

fn casegen_body(node: Uuid, module: &str) -> Value {
    use base64::Engine as _;
    json!({
        "module": module,
        "doc": "# 需求\n用户可以进入语聊房。账号 {{account.username}}。",
        "ui_links": ["https://example.com/design/1"],
        "screenshots": [{"data_b64": base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nxxxx")}],
        "node_id": node,
    })
}

#[tokio::test]
async fn 模块和生成请求_必填校验_去重_只能用自己的节点() {
    let world = World::new().await;
    let path = "/api/projects/hafa/casegen-requests";
    // 必填缺项：400，并说明缺什么
    for (key, value, expect) in [
        ("module", json!(""), "模块必填"),
        ("doc", json!(""), "需求文档必填"),
        ("ui_links", json!([]), "UI 图链接必填"),
        ("screenshots", json!([]), "UI 区域截图必填"),
        ("ui_links", json!(["javascript:alert(1)"]), "http"),
        ("doc", json!("密码：hunter2"), "明文密码"),
    ] {
        let mut body = casegen_body(world.node, "语聊房");
        body[key] = value;
        let (status, _, bytes) = browser_post(&world.app, path, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key}");
        assert!(text(&bytes).contains(expect), "{key}：{}", text(&bytes));
        assert!(!text(&bytes).contains("hunter2"), "错误里不能回显密码");
    }
    // 项目不存在：404；别人的令牌往这个节点发：403
    let (status, _, _) = browser_post(&world.app, "/api/projects/不存在的项目/casegen-requests", casegen_body(world.node, "m")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    sqlx::query("insert into users (name) values ('casegen-other')").execute(world.state.pool()).await.unwrap();
    let mut headers = review_headers(REVIEW_TOKEN);
    headers.push(("x-user-name", "casegen-other"));
    let (status, _, _) = request(&world.app, Method::POST, path, &headers, casegen_body(world.node, "语聊房").to_string()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // 成功：201；同模块同节点再提交：409
    let (status, _, bytes) = browser_post(&world.app, path, casegen_body(world.node, "语聊房")).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    let created = json_body(&bytes);
    let (status, _, bytes) = browser_post(&world.app, path, casegen_body(world.node, "语聊房")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", text(&bytes));
    // 另一个模块用同一份文档：项目级文档只存一份
    let (status, _, bytes) = browser_post(&world.app, path, casegen_body(world.node, "个人主页")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json_body(&bytes)["doc_id"], created["doc_id"]);
    let docs: i64 = sqlx::query_scalar("select count(*) from project_docs").fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(docs, 1);
    // 详情：截图数、链接、状态
    let id = created["request_id"].as_str().unwrap();
    let (status, _, bytes) = request(&world.app, Method::GET, &format!("/api/casegen-requests/{id}"), &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let detail = json_body(&bytes);
    assert_eq!((detail["status"].as_str(), detail["screenshot_count"].as_i64(), detail["module"].as_str()), (Some("queued"), Some(1), Some("语聊房")));
    assert_eq!(detail["ui_links"], json!(["https://example.com/design/1"]));
    // 模块列表带最近一次请求；老用例不属于任何模块
    let (status, _, bytes) = request(&world.app, Method::GET, "/api/projects/hafa/modules", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let modules = json_body(&bytes);
    assert_eq!(modules.as_array().unwrap().len(), 2);
    assert_eq!(modules[0]["latest_request"]["status"], "queued");
    assert_eq!(modules[0]["case_count"], 0);
    // 单独建模块：幂等；名字不合法 400
    let (status, _, _) = browser_post(&world.app, "/api/projects/hafa/modules", json!({"name": "  "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = browser_post(&world.app, "/api/projects/hafa/modules", json!({"name": "语聊房"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, _, bytes) = request(&world.app, Method::GET, "/api/projects/hafa/modules", &[], "").await;
    assert_eq!(json_body(&bytes).as_array().unwrap().len(), 2, "重复建同名模块不能多出一个");
}

#[tokio::test]
async fn 测试用例岗位只允许能看图的来源() {
    let world = World::new().await;
    let path = format!("/api/nodes/{}/roles", world.node);
    let post = |roles: Value| {
        let mut headers = review_headers(REVIEW_TOKEN);
        headers.push(("x-user-name", USER_NAME));
        let (app, path) = (world.app.clone(), path.clone());
        async move { request(&app, Method::POST, &path, &headers, set_roles_body(roles)).await }
    };
    let base = vec![role_json("e", "executor", "codex_exec", "m"), role_json("j", "judge", "agy", "m")];
    let with = |extra: Value| { let mut v = base.clone(); v.push(extra); json!(v) };
    // agy 的图片输入还没验证：不能当测试用例角色；deepseek 没有运行器
    for source in ["agy", "deepseek"] {
        let (status, _, bytes) = post(with(role_json("w", "case_writer", source, "m"))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{source}：{}", text(&bytes));
    }
    // Claude、Codex 能当：校验通过，只是测试里节点离线所以 409
    for source in ["claude_cli", "codex_exec"] {
        let (status, _, bytes) = post(with(role_json("w", "case_writer", source, "m"))).await;
        assert_eq!(status, StatusCode::CONFLICT, "{source}：{}", text(&bytes));
    }
    // 测试用例岗位可以不配（不是必需岗位）
    let (status, _, _) = post(json!(base)).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn 节点领取生成请求并回报_用例入库为草稿且不能执行() {
    let world = World::new().await;
    let node_headers = |node: Uuid| vec![("authorization", format!("Bearer {TOKEN}")), ("x-node-id", node.to_string())];
    let send = |method: Method, path: String, node: Uuid, body: String| {
        let (app, headers) = (world.app.clone(), node_headers(node));
        async move {
            let mut borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            borrowed.push(("content-type", "application/json"));
            request(&app, method, &path, &borrowed, body).await
        }
    };
    let (status, _, bytes) = browser_post(&world.app, "/api/projects/hafa/casegen-requests", casegen_body(world.node, "语聊房")).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    let id = json_body(&bytes)["request_id"].as_str().unwrap().to_string();
    // 别的节点领不到这个请求；自己的节点领到：带文档、链接、截图
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1,'{}',true)").bind(other).execute(world.state.pool()).await.unwrap();
    let (status, _, _) = send(Method::PUT, "/api/node/casegen-requests/claim".into(), other, String::new()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, bytes) = send(Method::PUT, "/api/node/casegen-requests/claim".into(), world.node, String::new()).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    let claimed = json_body(&bytes);
    assert_eq!((claimed["id"].as_str(), claimed["module"].as_str(), claimed["project"].as_str()), (Some(id.as_str()), Some("语聊房"), Some("hafa")));
    assert!(claimed["doc"].as_str().unwrap().contains("语聊房"));
    assert_eq!(claimed["screenshots"].as_array().unwrap().len(), 1);
    assert_eq!(claimed["screenshots"][0]["content_type"], "image/png");
    // 已被领走：再领是空的；别的节点不能回报
    let (status, _, _) = send(Method::PUT, "/api/node/casegen-requests/claim".into(), world.node, String::new()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let good = "id: G1\ntitle: 进入语聊房\nlevel: 简单流程\nplatform: [android]\nsteps:\n  - 点击语聊房入口\nexpected:\n  - 出现语聊房页面\n";
    let body = |ok: bool, text: &str| json!({"ok": ok, "detail": "测试", "cases_text": text}).to_string();
    let result = format!("/api/casegen-requests/{id}/result");
    let (status, _, _) = send(Method::PUT, result.clone(), other, body(true, good)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // 格式不合格：整批不入库，请求转失败并说明
    let (status, _, bytes) = send(Method::PUT, result.clone(), world.node, body(true, "id: G1\ntitle: 没步骤\nexpected:\n  - x\n")).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    assert_eq!(json_body(&bytes)["status"], "failed");
    let n: i64 = sqlx::query_scalar("select count(*) from test_cases where origin = 'generated'").fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(n, 0, "不合格的整批不能入库");
    // 失败后可以重新提交（失败的请求不再占着「同模块只能一个」）并成功
    let (status, _, bytes) = browser_post(&world.app, "/api/projects/hafa/casegen-requests", casegen_body(world.node, "语聊房")).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    let id2 = json_body(&bytes)["request_id"].as_str().unwrap().to_string();
    let (status, _, _) = send(Method::PUT, "/api/node/casegen-requests/claim".into(), world.node, String::new()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, bytes) = send(Method::PUT, format!("/api/casegen-requests/{id2}/result"), world.node, body(true, good)).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    let done = json_body(&bytes);
    assert_eq!(done["status"], "finished");
    let code = done["codes"][0].as_str().unwrap().to_string();
    assert!(code.starts_with("GEN-"), "{code}");
    let row: (String, String, String, bool) = sqlx::query_as("select origin, review_status, status, module_id is not null from test_cases where code = $1").bind(&code).fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(row, ("generated".into(), "draft".into(), "active".into(), true));
    // 草稿不能执行：创建任务被拒并说明原因；老用例不受影响
    let (status, _, bytes) = browser_post(&world.app, "/api/tasks", payload(world.node, &[&code])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("还没有通过复核和人工确认"), "{}", text(&bytes));
    let (status, _, _) = browser_post(&world.app, "/api/tasks", payload(world.node, &["TC-04"])).await;
    assert_eq!(status, StatusCode::CREATED);
    // 模块列表能看到草稿数量
    let (_, _, bytes) = request(&world.app, Method::GET, "/api/projects/hafa/modules", &[], "").await;
    assert_eq!(json_body(&bytes)[0]["case_count"], 1);
    assert_eq!(json_body(&bytes)[0]["approved_count"], 0);
}

#[tokio::test]
async fn 生成请求按节点上报的测试用例角色校验() {
    let world = World::new().await;
    let path = "/api/projects/hafa/casegen-requests";
    let writer = |id: &str, status| report_role(id, protocol::RoleSlot::CaseWriter, ModelSource::ClaudeCli, "m", &[], None, status);
    // 节点上报了角色但没有测试用例角色：拒绝并提示去配置
    world.state.set_roles_report_for_test(world.node, protocol::RolesReport { roles: vec![report_role("e", protocol::RoleSlot::Executor, ModelSource::CodexExec, "m", &["high"], Some("high"), protocol::CheckStatus::Ok)] });
    let (status, _, bytes) = browser_post(&world.app, path, casegen_body(world.node, "语聊房")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("还没有配置「测试用例」角色"), "{}", text(&bytes));
    // 有但没检测通过：拒绝；指定了没检测通过的角色：拒绝并说明是哪个，不退回别的
    world.state.set_roles_report_for_test(world.node, protocol::RolesReport { roles: vec![writer("w-bad", protocol::CheckStatus::Failed), writer("w-ok", protocol::CheckStatus::Ok)] });
    let mut body = casegen_body(world.node, "语聊房");
    body["writer_role"] = json!("w-bad");
    let (status, _, bytes) = browser_post(&world.app, path, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("还没有检测通过") && text(&bytes).contains("角色-w-bad"), "{}", text(&bytes));
    let mut body = casegen_body(world.node, "语聊房");
    body["writer_role"] = json!("不存在");
    assert_eq!(browser_post(&world.app, path, body).await.0, StatusCode::BAD_REQUEST);
    // 不指定：用第一个检测通过的，并记在请求里
    let (status, _, bytes) = browser_post(&world.app, path, casegen_body(world.node, "语聊房")).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    let id: Uuid = json_body(&bytes)["request_id"].as_str().unwrap().parse().unwrap();
    let stored: Option<String> = sqlx::query_scalar("select writer_role from casegen_requests where id = $1").bind(id).fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(stored.as_deref(), Some("w-ok"));
}

/// 建一个模块并让节点生成两条草稿用例，返回（模块 id、请求 id、两条用例编号）。
async fn generated_module(world: &World, module: &str) -> (Uuid, Uuid, Vec<String>) {
    let node_headers = vec![("authorization", format!("Bearer {TOKEN}")), ("x-node-id", world.node.to_string()), ("content-type", "application/json".to_string())];
    let send = |method: Method, path: String, body: String| {
        let (app, headers) = (world.app.clone(), node_headers.clone());
        async move {
            let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            request(&app, method, &path, &borrowed, body).await
        }
    };
    let (status, _, bytes) = browser_post(&world.app, "/api/projects/hafa/casegen-requests", casegen_body(world.node, module)).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    let created = json_body(&bytes);
    let request_id: Uuid = created["request_id"].as_str().unwrap().parse().unwrap();
    let module_id: Uuid = created["module_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(send(Method::PUT, "/api/node/casegen-requests/claim".into(), String::new()).await.0, StatusCode::OK);
    let two = "id: G1\ntitle: 进入语聊房\nsteps:\n  - 点击入口\nexpected:\n  - 出现语聊房\n---\nid: G2\ntitle: 退出语聊房\nsteps:\n  - 点击退出\nexpected:\n  - 回到首页\n";
    let (status, _, bytes) = send(Method::PUT, format!("/api/casegen-requests/{request_id}/result"), json!({"ok": true, "detail": "", "cases_text": two}).to_string()).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    let done = json_body(&bytes);
    // 回报成功时把用例摘要给节点，评审员据此复核
    assert_eq!(done["cases"].as_array().unwrap().len(), 2);
    assert_eq!(done["cases"][0]["expected"], json!(["出现语聊房"]));
    let codes = done["codes"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
    (module_id, request_id, codes)
}

fn node_put(world: &World, node: Uuid, path: String, body: Value) -> impl std::future::Future<Output = (StatusCode, HeaderMap, Bytes)> {
    let app = world.app.clone();
    async move {
        let headers = [("authorization", format!("Bearer {TOKEN}")), ("x-node-id", node.to_string()), ("content-type", "application/json".to_string())];
        let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        request(&app, Method::PUT, &path, &borrowed, body.to_string()).await
    }
}

async fn status_of_case(world: &World, code: &str) -> (String, String) {
    sqlx::query_as("select review_status, review_note from test_cases where code = $1").bind(code).fetch_one(world.state.pool()).await.unwrap()
}

#[tokio::test]
async fn 评审员的意见只是意见_要人确认才能执行() {
    let world = World::new().await;
    let (module, request_id, codes) = generated_module(&world, "语聊房").await;
    let review_path = format!("/api/casegen-requests/{request_id}/review");
    // 别的节点不能代报
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1,'{}',true)").bind(other).execute(world.state.pool()).await.unwrap();
    assert_eq!(node_put(&world, other, review_path.clone(), json!({"ok": true, "detail": "", "reviews": []})).await.0, StatusCode::CONFLICT);
    // 评审员：第一条没问题，第二条有疑问；结论不认识的和没给结论的都转待人工确认
    let (status, _, bytes) = node_put(&world, world.node, review_path.clone(), json!({
        "ok": true, "detail": "评审员 Claude 复核完成",
        "reviews": [{"code": codes[0], "verdict": "ok", "reason": "文档第 2 段能对上"}, {"code": codes[1], "verdict": "needs_human", "reason": "文档没写退出后回哪个页面"}],
    })).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    assert_eq!(status_of_case(&world, &codes[0]).await, ("reviewed_ok".into(), "文档第 2 段能对上".into()));
    assert_eq!(status_of_case(&world, &codes[1]).await.0, "needs_human");
    // 评审员说没问题也不能直接执行：还不是 approved
    let (status, _, bytes) = browser_post(&world.app, "/api/tasks", payload(world.node, &[&codes[0]])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("reviewed_ok"), "{}", text(&bytes));
    // 再报一次不会改已经复核过的（只更新仍是草稿的）
    node_put(&world, world.node, review_path, json!({"ok": false, "detail": "x", "reviews": []})).await;
    assert_eq!(status_of_case(&world, &codes[0]).await.0, "reviewed_ok");
    // 人的决定：个人令牌必须是发起这个模块生成请求的人
    sqlx::query("insert into users (name) values ('unrelated')").execute(world.state.pool()).await.unwrap();
    let decide = |code: &str, token: &'static str, body: Value| {
        let mut h = review_headers(REVIEW_TOKEN);
        h.push(("x-user-name", token));
        let (app, path) = (world.app.clone(), format!("/api/modules/{module}/cases/{code}/decision"));
        async move { request(&app, Method::POST, &path, &h, body.to_string()).await }
    };
    assert_eq!(decide(&codes[0], "unrelated", json!({"decision": "approve"})).await.0, StatusCode::FORBIDDEN);
    assert_eq!(decide(&codes[0], "nobody", json!({"decision": "approve"})).await.0, StatusCode::FORBIDDEN);
    assert_eq!(decide(&codes[0], USER_NAME, json!({"decision": "maybe"})).await.0, StatusCode::BAD_REQUEST);
    // 评审员有疑问的用例，批准要写备注
    let (status, _, bytes) = decide(&codes[1], USER_NAME, json!({"decision": "approve"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("备注"));
    assert_eq!(decide(&codes[1], USER_NAME, json!({"decision": "approve", "note": "退出后回首页是产品口头确认的"})).await.0, StatusCode::NO_CONTENT);
    let (status, note) = status_of_case(&world, &codes[1]).await;
    assert_eq!(status, "approved");
    assert!(note.contains("文档没写退出后回哪个页面") && note.contains("产品口头确认"), "评审员的疑问和人的备注都要留痕：{note}");
    // 一键批准只批「评审员认为没问题」的
    let mut h = review_headers(REVIEW_TOKEN);
    h.push(("x-user-name", USER_NAME));
    let (status, _, bytes) = request(&world.app, Method::POST, &format!("/api/modules/{module}/approve-reviewed"), &h, "{}").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&bytes)["approved"], 1);
    assert_eq!(status_of_case(&world, &codes[0]).await.0, "approved");
    // 批准后才能执行；拒绝的不能
    let (status, _, bytes) = browser_post(&world.app, "/api/tasks", payload(world.node, &[&codes[0], &codes[1]])).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    assert_eq!(decide(&codes[1], USER_NAME, json!({"decision": "reject", "note": "不要了"})).await.0, StatusCode::NO_CONTENT);
    let (status, _, _) = browser_post(&world.app, "/api/tasks", payload(world.node, &[&codes[1]])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // 模块的用例列表带复核状态和备注
    let (_, _, bytes) = request(&world.app, Method::GET, &format!("/api/modules/{module}/cases"), &[], "").await;
    let list = json_body(&bytes);
    assert_eq!((list[0]["review_status"].as_str(), list[1]["review_status"].as_str()), (Some("approved"), Some("rejected")));
    // 执行过的用例带最近一次执行；没执行过的是 null。有排队中的 run 时模块就是「测试中」
    assert_eq!(list[0]["latest_run"]["status"], "queued");
    assert!(list[1]["latest_run"]["run_id"].is_string(), "被拒绝前执行过一次，仍能看到最近一次执行");
    let (_, _, bytes) = request(&world.app, Method::GET, "/api/projects/hafa/modules", &[], "").await;
    assert_eq!(json_body(&bytes)[0]["active_runs"], 2);
}

#[tokio::test]
async fn 评审员没跑成时用例全部转待人工确认() {
    let world = World::new().await;
    let (_, request_id, codes) = generated_module(&world, "个人主页").await;
    let (status, _, _) = node_put(&world, world.node, format!("/api/casegen-requests/{request_id}/review"), json!({"ok": false, "detail": "这个节点没有配置判定员角色", "reviews": []})).await;
    assert_eq!(status, StatusCode::OK);
    for code in &codes {
        let (status, note) = status_of_case(&world, code).await;
        assert_eq!(status, "needs_human", "评审员没跑成不能当通过");
        assert!(note.contains("没有跑成") && note.contains("没有配置判定员角色"), "{note}");
    }
}

#[tokio::test]
async fn 应用环境检查接口_查看公开_检查只能是节点主人_离线不返回结果() {
    let world = World::new().await;
    let apps = format!("/api/nodes/{}/apps", world.node);
    // 查看是公开的：不填名字也能看（只是 mine 为空）
    let (status, _, bytes) = request(&world.app, Method::GET, &apps, &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json_body(&bytes)["mine"].is_null());
    // 放一条检查结果：节点没有 WebSocket 连接（离线），不返回上一次的（可能过期的）结果，但四个应用都在
    world.state.set_app_scan_for_test(world.node, protocol::AppScan {
        source: ModelSource::Agy, installed: true, version: Some("1.0".into()), login: protocol::AppLogin::Ok,
        login_detail: "已用个人账号登录".into(),
        models: vec![protocol::AppModel { id: "gemini-x".into(), display: "Gemini X".into(), efforts: vec![], default_effort: None }],
        models_note: "来自 agy models".into(), scanned_at: "2026-11-23T00:00:00Z".into(),
    });
    let (status, headers, bytes) = request(&world.app, Method::GET, &apps, &user_headers(USER_NAME), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let view = json_body(&bytes);
    assert_eq!(view["online"], false);
    let sources: Vec<&str> = view["apps"].as_array().unwrap().iter().map(|a| a["source"].as_str().unwrap()).collect();
    assert_eq!(sources, ["claude_cli", "codex_exec", "agy", "deepseek"]);
    assert!(view["apps"].as_array().unwrap().iter().all(|a| a["scanned"] == false), "离线时不返回上一次的结果");
    // 触发检查：不认识的应用 404；离线 409；别人的节点 403
    let scan = |source: &str, token: &'static str| {
        let mut h = review_headers(REVIEW_TOKEN);
        h.push(("x-user-name", token));
        let (app, path) = (world.app.clone(), format!("{apps}/{source}/scan"));
        async move { request(&app, Method::POST, &path, &h, "{}").await.0 }
    };
    assert_eq!(scan("不存在的应用", USER_NAME).await, StatusCode::NOT_FOUND);
    assert_eq!(scan("agy", USER_NAME).await, StatusCode::CONFLICT);
    sqlx::query("insert into users (name) values ('apps-other')").execute(world.state.pool()).await.unwrap();
    assert_eq!(scan("agy", "apps-other").await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn 数据大家共用_我可以用自己的节点和额度跑别人的用例_但不能用别人的节点() {
    let world = World::new().await;
    // 测试用户生成并批准了一批用例；另一个人（runner）有自己的节点
    let (module, _, codes) = generated_module(&world, "共用的模块").await;
    sqlx::query("update test_cases set review_status = 'approved' where module_id = $1").bind(module).execute(world.state.pool()).await.unwrap();
    let runner: Uuid = sqlx::query_scalar("insert into users (name) values ('runner') returning id").fetch_one(world.state.pool()).await.unwrap();
    let runner_node = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online, owner_user_id) values ($1, $2, true, $3)")
        .bind(runner_node)
        .bind(json!({"model_sources": [{"source": "claude_cli"}, {"source": "codex_exec"}]}))
        .bind(runner)
        .execute(world.state.pool())
        .await
        .unwrap();
    sqlx::query("insert into devices (node_id, serial, platform, status) values ($1,'runner设备','android','idle')").bind(runner_node).execute(world.state.pool()).await.unwrap();
    let run_as = |name: &'static str, node: Uuid| {
        let mut h = review_headers(REVIEW_TOKEN);
        h.push(("x-user-name", name));
        let (app, body) = (world.app.clone(), payload(node, &[&codes[0]]).to_string());
        async move { request(&app, Method::POST, "/api/tasks", &h, body).await }
    };
    // 数据是公开的：不填名字也能看到别人的模块和用例
    let (status, _, bytes) = request(&world.app, Method::GET, &format!("/api/modules/{module}/cases"), &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&bytes)[0]["review_status"], "approved");
    // runner 用自己的节点跑测试用户的用例：可以，任务落在 runner 自己的节点上（花的是 runner 的额度）
    let (status, _, bytes) = run_as("runner", runner_node).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    let node_of_run: Uuid = sqlx::query_scalar("select node_id from tasks order by created_at desc limit 1").fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(node_of_run, runner_node);
    // 但 runner 不能把任务派到测试用户的节点（那是用别人的额度）
    let (status, _, bytes) = run_as("runner", world.node).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", text(&bytes));
    assert!(text(&bytes).contains("各人自己的额度"), "{}", text(&bytes));
}

struct World {
    temp: TempPaths,
    app: Router,
    state: AppState,
    node: Uuid,
}

impl World {
    async fn new() -> Self {
        Self::with_state(with_token(test_state(config()).await, Some(REVIEW_TOKEN))).await
    }

    async fn with_state(state: AppState) -> Self {
        let temp = TempPaths::with_config();
        let app = temp.app(state.clone());
        let node = Uuid::new_v4();
        let caps = json!({"model_sources": [{"source": "claude_cli"}, {"source": "codex_exec"}]});
        // 测试用户（只记名字）；节点属于该用户
        let user: Uuid = sqlx::query_scalar("insert into users (name) values ('测试用户') returning id")
            .fetch_one(state.pool())
            .await
            .unwrap();
        sqlx::query("insert into nodes (id, caps, online, owner_user_id) values ($1,$2,true,$3)")
            .bind(node)
            .bind(caps)
            .bind(user)
            .execute(state.pool())
            .await
            .unwrap();
        sqlx::query("insert into devices (node_id, serial, platform, status) values ($1,'测试设备','android','idle')")
            .bind(node).execute(state.pool()).await.unwrap();
        let case = NewCase {
            code: "TC-04".into(),
            title: "切换选中状态".into(),
            level: "简单流程".into(),
            platforms: vec!["android".into()],
            preconditions: vec![],
            steps: vec!["点击选择框".into()],
            expected: vec![CaseExpected {
                idx: 1,
                text: "出现选中标记".into(),
            }],
            data: json!({"用途": "仅供节点"}),
            refs: vec![],
            note: String::new(),
            should_fail: false,
        };
        let another = NewCase {
            code: "TC-05".into(),
            ..case.clone()
        };
        let blocked = NewCase {
            code: "TC-07".into(),
            ..case.clone()
        };
        cases::import(
            state.pool(),
            "hafa",
            &[case, another, blocked],
            &BTreeMap::from([("TC-07".into(), "设备不支持该操作".into())]),
        )
        .await
        .unwrap();
        Self {
            temp,
            app,
            state,
            node,
        }
    }

    async fn create(&self, codes: &[&str]) -> (Uuid, Vec<Uuid>) {
        let (status, _, body) =
            browser_post(&self.app, "/api/tasks", payload(self.node, codes)).await;
        assert_eq!(status, StatusCode::CREATED, "{}", text(&body));
        let body = json_body(&body);
        (
            serde_json::from_value(body["task_id"].clone()).unwrap(),
            serde_json::from_value(body["run_ids"].clone()).unwrap(),
        )
    }

    async fn run(&self) -> Uuid {
        self.create(&["TC-04"]).await.1[0]
    }

    fn disk_file(&self, run_id: Uuid, name: &str) -> PathBuf {
        self.temp
            .paths
            .data
            .join("files")
            .join(run_id.to_string())
            .join(name)
    }
}

#[path = "reviews_api_tests.rs"]
mod reviews_api_tests;

#[path = "scripts_api_tests.rs"]
mod scripts_api_tests;

#[tokio::test]
async fn 创建任务业务校验失败返回400及原因() {
    let world = World::new().await;
    let body = payload(world.node, &["TC-04", "TC-07"]);
    let (status, _, bytes) = browser_post(&world.app, "/api/tasks", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("设备不支持该操作"));
    sqlx::query("update nodes set caps = '{\"model_sources\":[{\"source\":\"codex_exec\"}]}'::jsonb where id = $1")
        .bind(world.node).execute(world.state.pool()).await.unwrap();
    let (status, _, bytes) =
        browser_post(&world.app, "/api/tasks", payload(world.node, &["TC-04"])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(text(&bytes).contains("没有可用的 claude_cli"));
    let count: i64 = sqlx::query_scalar("select count(*) from tasks")
        .fetch_one(world.state.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn 创建任务只能用自己的节点() {
    let world = World::new().await;
    let post_as = |token: Option<&'static str>, node: Uuid| {
        let app = world.app.clone();
        async move {
            let mut headers = review_headers(REVIEW_TOKEN);
            if let Some(token) = token {
                headers.push(("x-user-name", token));
            }
            request(&app, Method::POST, "/api/tasks", &headers, payload(node, &["TC-04"]).to_string()).await.0
        }
    };
    assert_eq!(post_as(None, world.node).await, StatusCode::UNAUTHORIZED, "没带个人令牌");
    assert_eq!(post_as(Some("nobody"), world.node).await, StatusCode::UNAUTHORIZED, "令牌无效");
    // 另一个用户拿自己的有效令牌，不能用别人的节点
    let other_token = "other-name";
    sqlx::query("insert into users (name) values ('other-name')")
        .execute(world.state.pool())
        .await
        .unwrap();
    assert_eq!(post_as(Some(other_token), world.node).await, StatusCode::FORBIDDEN, "不是自己的节点");
    // 还没有属主的节点谁都不能用
    sqlx::query("update nodes set owner_user_id = null where id = $1").bind(world.node).execute(world.state.pool()).await.unwrap();
    assert_eq!(post_as(Some(USER_NAME), world.node).await, StatusCode::FORBIDDEN, "无属主节点");
    let count: i64 = sqlx::query_scalar("select count(*) from tasks").fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(count, 0, "被拒绝的请求不能留下任务");
}

#[tokio::test]
async fn 创建任务成功固定模型来源并可查询任务与run时间() {
    let world = World::new().await;
    let (task, runs) = world.create(&["TC-04", "TC-05"]).await;
    assert_eq!(runs.len(), 2);
    let pool = world.state.pool();
    let claimed = runs::claim_next(pool, world.node).await.unwrap().unwrap();
    assert_eq!(claimed.run_id, runs[0]);
    runs::mark_started(pool, world.node, runs[0]).await.unwrap();
    runs::cancel(pool, runs[0]).await.unwrap();
    let (status, _, bytes) = request(
        &world.app,
        Method::GET,
        &format!("/api/tasks/{task}"),
        &[],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let detail = json_body(&bytes);
    assert_eq!(detail["task_id"], json!(task));
    assert_eq!(
        detail["executor"],
        json!({"source": "codex_exec", "model": "测试执行模型", "effort": "max"})
    );
    assert_eq!(
        detail["judge"],
        json!({"source": "claude_cli", "model": "测试判定模型", "effort": null})
    );
    let first = &detail["runs"][0];
    assert_eq!(first["device_serial"], "测试设备");
    for key in ["queued_at", "started_at", "finished_at"] {
        assert!(first[key].is_string());
    }
    assert!(detail["runs"][1]["device_serial"].is_null());

    let (status, _, bytes) = request(&world.app, Method::GET, "/api/tasks?limit=1", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let list = json_body(&bytes);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["task_id"], json!(task));
    assert_eq!(list[0]["node_id"], json!(world.node));
    assert_eq!(list[0]["project"], "hafa");
    assert_eq!(list[0]["run_count"], 2);
    assert_eq!(list[0]["status_counts"]["cancelled"], 1);
    assert_eq!(list[0]["status_counts"]["queued"], 1);
    assert_eq!(list[0]["status_counts"]["blocked"], 0);
    assert!(list[0]["created_at"].is_string());
    assert_eq!(list[0]["result_summary"], "已取消 1、进行中 1");
}

#[test]
fn 任务列表判定计数序列化保留零值() {
    let verdict_counts = VerdictCounts {
        passed: 2,
        failed: 0,
        uncertain: 3,
        blocked: 0,
        cancelled: 0,
        running: 4,
    };
    assert_eq!(
        serde_json::to_value(&verdict_counts).unwrap(),
        json!({"passed": 2, "failed": 0, "uncertain": 3, "blocked": 0, "cancelled": 0, "running": 4})
    );
    assert_eq!(
        verdict_counts.result_summary(),
        "通过 2、待人工确认 3、进行中 4"
    );
}

#[tokio::test]
async fn 任务列表判定计数与现有摘要同源() {
    let world = World::new().await;
    let pool = world.state.pool();
    let task_id: Uuid = sqlx::query_scalar(
        "insert into tasks (project_id, node_id, executor, judge)
         select id, $1, '{\"source\":\"codex_exec\",\"model\":null}', '{\"source\":\"claude_cli\",\"model\":null}'
         from projects where name = 'hafa' returning id",
    )
    .bind(world.node)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "with test_devices as (
             insert into devices (node_id, serial, platform, status)
             select t.node_id, active.status, 'android', 'busy'
             from tasks t cross join (values ('assigned'), ('running'), ('judging'), ('node_lost')) active(status)
             where t.id = $1 returning id, serial
         )
         insert into runs (task_id, case_id, case_version, case_spec, node_id, device_id, status, final_result, blocked_reason)
         select t.id, c.id, c.version, '{}', t.node_id, d.id, fixture.status, fixture.final_result, fixture.blocked_reason
         from tasks t join test_cases c on c.project_id = t.project_id and c.code = 'TC-04'
         cross join (values ('finished', 'passed', null), ('finished', 'passed', null),
                            ('finished', 'failed', null), ('finished', 'uncertain', null),
                            ('blocked', 'blocked', 'other'), ('cancelled', null, null), ('queued', null, null),
                            ('assigned', null, null), ('running', null, null),
                            ('judging', null, null), ('node_lost', null, null)) fixture(status, final_result, blocked_reason)
         left join test_devices d on d.serial = fixture.status
         where t.id = $1",
    )
    .bind(task_id)
    .execute(pool)
    .await
    .unwrap();

    let (status, _, bytes) = request(&world.app, Method::GET, "/api/tasks", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let list = json_body(&bytes);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["task_id"], json!(task_id));
    assert_eq!(list[0]["run_count"], 11);
    assert_eq!(list[0]["status_counts"]["running"], 1);
    assert_eq!(
        list[0]["verdict_counts"],
        json!({"passed": 2, "failed": 1, "uncertain": 1, "blocked": 1, "cancelled": 1, "running": 5})
    );
    assert_eq!(
        list[0]["result_summary"],
        "通过 2、失败 1、待人工确认 1、受阻 1、已取消 1、进行中 5"
    );
}

#[tokio::test]
async fn 任务列表默认30条且按创建时间倒序() {
    let world = World::new().await;
    sqlx::query(
        "insert into tasks (project_id, node_id, executor, judge, created_at)
         select p.id, $1, '{\"source\":\"codex_exec\",\"model\":null}', '{\"source\":\"claude_cli\",\"model\":null}',
                now() - make_interval(secs => g.n)
         from projects p cross join generate_series(1, 31) g(n) where p.name = 'hafa'",
    )
    .bind(world.node).execute(world.state.pool()).await.unwrap();
    let (status, _, bytes) = request(&world.app, Method::GET, "/api/tasks", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let list = json_body(&bytes);
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 30);
    assert!(
        list.windows(2)
            .all(|pair| pair[0]["created_at"].as_str() > pair[1]["created_at"].as_str())
    );
}

#[tokio::test]
async fn 重复取消终态run返回409() {
    let world = World::new().await;
    let run = world.run().await;
    let path = format!("/api/runs/{run}/cancel");
    assert_eq!(
        browser_post(&world.app, &path, json!({})).await.0,
        StatusCode::NO_CONTENT
    );
    let (status, _, bytes) = browser_post(&world.app, &path, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(text(&bytes).contains("不能取消"));
}

#[tokio::test]
async fn 正确口令可创建并取消执行且无口令可查看详情() {
    let world = World::new().await;
    let (task, runs) = world.create(&["TC-04"]).await;
    assert_eq!(runs.len(), 1);
    let run = runs[0];
    let (status, _, body) =
        browser_post(&world.app, &format!("/api/runs/{run}/cancel"), json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty());

    let (status, _, body) =
        request(&world.app, Method::GET, &format!("/api/runs/{run}"), &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let detail = json_body(&body);
    assert_eq!(detail["task_id"], json!(task));
    assert_eq!(detail["status"], "cancelled");
}

/// 不带签名的文件路径，用于节点上传（上传走节点令牌，不走签名）。
fn bare_file_route(run: Uuid, name: &str) -> String {
    format!("/api/runs/{run}/files/{name}")
}

/// 带有效签名的读取地址：读文件必须验签，测试用进程内同一把签名密钥生成。
fn file_route(run: Uuid, name: &str) -> String {
    let exp = chrono::Utc::now().timestamp() + crate::evidence_sign::TTL_SECS;
    crate::evidence_sign::process_signer().url(run, name, exp)
}

/// 名字含百分号编码时：服务端对解码后的名字验签，所以对解码名签名，路径里放编码名。
fn file_route_encoded(run: Uuid, decoded: &str, encoded: &str) -> String {
    let signed = file_route(run, decoded);
    let query = signed.split_once('?').expect("签名地址带查询串").1;
    format!("{}?{query}", bare_file_route(run, encoded))
}

#[tokio::test]
async fn 文件读取没有有效签名一律拒绝() {
    let world = World::new().await;
    let run = world.run().await;
    assert_eq!(
        upload(&world.app, run, world.node, "sig.png", PNG.to_vec()).await.0,
        StatusCode::NO_CONTENT
    );
    // 签名正确时可读，作为对照。
    assert_eq!(
        request(&world.app, Method::GET, &file_route(run, "sig.png"), &[], "").await.0,
        StatusCode::OK
    );
    let signed = file_route(run, "sig.png");
    let bare = bare_file_route(run, "sig.png");
    let tampered = signed.replacen("sig=", "sig=00", 1);
    for url in [bare, tampered] {
        assert_eq!(
            request(&world.app, Method::GET, &url, &[], "").await.0,
            StatusCode::FORBIDDEN,
            "没有有效签名必须拒绝：{url}"
        );
    }
}

async fn upload(
    app: &Router,
    run: Uuid,
    node: Uuid,
    name: &str,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Bytes) {
    request(
        app,
        Method::PUT,
        &bare_file_route(run, name),
        &[
            ("authorization", &format!("Bearer {TOKEN}")),
            ("x-node-id", &node.to_string()),
        ],
        body,
    )
    .await
}

#[tokio::test]
async fn 文件读取拒绝路径穿越() {
    let temp = TempPaths::new();
    let app = temp.app(lazy_state());
    // (解码后的名字, 路径里的编码写法)：签名有效但文件名非法，仍必须返回 404。
    for (decoded, encoded) in [
        ("../other.png", "%2e%2e%2fother.png"),
        ("shot\\other.png", "shot%5Cother.png"),
        ("shot..png", "shot..png"),
        ("sub/file.png", "sub/file.png"),
        ("终态.txt", "%E7%BB%88%E6%80%81.txt"),
    ] {
        let path = file_route_encoded(Uuid::new_v4(), decoded, encoded);
        assert_eq!(
            request(&app, Method::GET, &path, &[], "").await.0,
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn 文件读取必须先在数据库登记() {
    let world = World::new().await;
    let run = world.run().await;
    let path = world.disk_file(run, "local.png");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, PNG).unwrap();
    assert_eq!(
        request(
            &world.app,
            Method::GET,
            &file_route(run, "local.png"),
            &[],
            ""
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[cfg(unix)]
#[tokio::test]
async fn 文件读取拒绝符号链接逃逸() {
    use std::os::unix::fs::symlink;
    let world = World::new().await;
    let run = world.run().await;
    let name = "escape.png";
    assert_eq!(
        upload(&world.app, run, world.node, name, PNG.to_vec())
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let disk = world.disk_file(run, name);
    std::fs::remove_file(&disk).unwrap();
    let outside = world.temp.root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join(name), PNG).unwrap();
    symlink(outside.join(name), &disk).unwrap();
    let url = file_route(run, name);
    assert_eq!(
        request(&world.app, Method::GET, &url, &[], "").await.0,
        StatusCode::NOT_FOUND
    );
    std::fs::remove_dir_all(disk.parent().unwrap()).unwrap();
    symlink(&outside, disk.parent().unwrap()).unwrap();
    assert_eq!(
        request(&world.app, Method::GET, &url, &[], "").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn 文件上传令牌错误返回401() {
    let temp = TempPaths::new();
    let app = temp.app(lazy_state());
    let path = file_route(Uuid::new_v4(), "shot.png");
    let (status, _, bytes) = request(
        &app,
        Method::PUT,
        &path,
        &[("authorization", "Bearer invalid")],
        PNG.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(bytes.is_empty());
}

#[tokio::test]
async fn 文件上传节点不符返回403() {
    let world = World::new().await;
    let run = world.run().await;
    let (status, _, bytes) =
        upload(&world.app, run, Uuid::new_v4(), "shot.png", PNG.to_vec()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(bytes.is_empty());
    assert!(!world.disk_file(run, "shot.png").exists());
}

#[tokio::test]
async fn 文件上传拒绝错误魔数无效_utf8_和非法名称() {
    let world = World::new().await;
    let run = world.run().await;
    for (name, bytes) in [
        ("shot.png", "不是图片".as_bytes().to_vec()),
        ("final_page.txt", vec![0xff]),
        ("bad..png", PNG.to_vec()),
        ("file.html", vec![]),
    ] {
        assert_eq!(
            upload(&world.app, run, world.node, name, bytes).await.0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn 文件上传拒绝超过十兆字节() {
    let world = World::new().await;
    let run = world.run().await;
    let bytes = vec![b'a'; files::MAX_SIZE + 1];
    assert_eq!(
        upload(&world.app, run, world.node, "final_page.txt", bytes)
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn 文件上传原子覆盖幂等且可读回() {
    let world = World::new().await;
    let run = world.run().await;
    let name = "01_after.png";
    let old = [PNG, b"first"].concat();
    let new = [PNG, b"second"].concat();
    assert_eq!(
        upload(&world.app, run, world.node, name, old.clone())
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let disk = world.disk_file(run, name);
    let mut opened_before_overwrite = std::fs::File::open(&disk).unwrap();
    let mut results = Vec::new();
    for bytes in [&new, &new] {
        results.push(upload(&world.app, run, world.node, name, bytes.clone()));
    }
    for (status, _, _) in futures_util::future::join_all(results).await {
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let mut previous = Vec::new();
    opened_before_overwrite.read_to_end(&mut previous).unwrap();
    assert_eq!(previous, old, "已打开的文件仍应读到完整旧内容");
    assert_eq!(
        std::fs::read_dir(disk.parent().unwrap()).unwrap().count(),
        1
    );
    let (status, headers, body) =
        request(&world.app, Method::GET, &file_route(run, name), &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), new);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let records: Vec<(String, i64, String)> =
        sqlx::query_as("select kind, size, sha256 from files where run_id = $1")
            .bind(run)
            .fetch_all(world.state.pool())
            .await
            .unwrap();
    assert_eq!(
        records,
        [(
            "shot".into(),
            new.len() as i64,
            hex::encode(Sha256::digest(&new))
        )]
    );
    assert_eq!(
        upload(&world.app, run, world.node, "01_AFTER.png", PNG.to_vec())
            .await
            .0,
        StatusCode::CONFLICT
    );

    let page = "终态文字";
    assert_eq!(
        upload(
            &world.app,
            run,
            world.node,
            "final_page.txt",
            page.as_bytes().to_vec()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (status, headers, body) = request(
        &world.app,
        Method::GET,
        &file_route(run, "final_page.txt"),
        &[],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert_eq!(text(&body), page);
}

async fn put_index(world: &World, run: Uuid, node: Uuid, segments: Value) -> StatusCode {
    request(
        &world.app, Method::PUT, &format!("/api/runs/{run}/recording-index"),
        &[("authorization", &format!("Bearer {TOKEN}")), ("x-node-id", &node.to_string()), ("content-type", "application/json")],
        json!({"segments": segments}).to_string(),
    ).await.0
}

#[tokio::test]
async fn 录屏验证文件名魔数且有独立的一百五十兆上限() {
    let world = World::new().await;
    let run = world.run().await;
    for (name, body) in [("rec_01.mp4", b"ftypnot-mp4".to_vec()), ("rec_1.mp4", MP4.to_vec()), ("rec_00.mp4", MP4.to_vec())] {
        assert_eq!(upload(&world.app, run, world.node, name, body).await.0, StatusCode::BAD_REQUEST);
    }
    let mut large = MP4.to_vec();
    large.resize(files::MAX_SIZE + 1, 0);
    assert_eq!(upload(&world.app, run, world.node, "rec_01.mp4", large).await.0, StatusCode::NO_CONTENT);
    let path = file_route(run, "rec_02.mp4");
    let (status, _, _) = request(&world.app, Method::PUT, &path, &[
        ("authorization", &format!("Bearer {TOKEN}")),
        ("x-node-id", &world.node.to_string()),
        ("content-length", &(protocol::recording::MAX_RECORDING_SIZE + 1).to_string()),
    ], MP4.to_vec()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!world.disk_file(run, "rec_02.mp4").exists());
}

#[tokio::test]
async fn 录屏上传和索引拒绝跨节点且不能引用其他run文件() {
    let world = World::new().await;
    let run = world.run().await;
    let other_run = world.run().await;
    let other_node = Uuid::new_v4();
    let segments = json!([{"seq": 1, "file": "rec_01.mp4", "start_epoch_ms": 1700000000000_i64, "duration_ms": 4000}]);
    let index_url = format!("/api/runs/{run}/recording-index");
    assert_eq!(request(&world.app, Method::PUT, &index_url, &[], "{}").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(upload(&world.app, run, other_node, "rec_01.mp4", MP4.to_vec()).await.0, StatusCode::FORBIDDEN);
    assert_eq!(put_index(&world, run, other_node, segments.clone()).await, StatusCode::FORBIDDEN);
    assert_eq!(upload(&world.app, run, world.node, "rec_01.mp4", MP4.to_vec()).await.0, StatusCode::NO_CONTENT);
    assert_eq!(put_index(&world, other_run, world.node, segments).await, StatusCode::CONFLICT);
    let disk = world.disk_file(other_run, "rec_01.mp4");
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(disk, MP4).unwrap();
    assert_eq!(request(&world.app, Method::GET, &file_route(other_run, "rec_01.mp4"), &[("range", "bytes=0-3")], "").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn 录屏播放支持完整和单区间读取并拒绝非法区间与路径穿越() {
    let world = World::new().await;
    let run = world.run().await;
    let name = "rec_01.mp4";
    let url = file_route(run, name);
    assert_eq!(upload(&world.app, run, world.node, name, MP4.to_vec()).await.0, StatusCode::NO_CONTENT);
    let (status, headers, body) = request(&world.app, Method::GET, &url, &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(headers[header::CONTENT_LENGTH], "16");
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(&body[..], MP4);
    for (range, content_range, expected) in [
        ("bytes=4-7", "bytes 4-7/16", &MP4[4..8]),
        ("bytes=10-", "bytes 10-15/16", &MP4[10..]),
        ("bytes=-3", "bytes 13-15/16", &MP4[13..]),
        ("bytes=4-9999999999999999999999999", "bytes 4-15/16", &MP4[4..]),
    ] {
        let (status, headers, body) = request(&world.app, Method::GET, &url, &[("range", range)], "").await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(headers[header::CONTENT_RANGE], content_range);
        assert_eq!(headers[header::CONTENT_LENGTH], expected.len().to_string());
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(headers[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(&body[..], expected);
    }
    for range in ["bytes=16-", "bytes=8-3", "bytes=1-2,4-5", "bytes=-0", "bytes=x-", "items=0-1"] {
        let (status, headers, body) = request(&world.app, Method::GET, &url, &[("range", range)], "").await;
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(headers[header::CONTENT_RANGE], "bytes */16");
        assert!(body.is_empty());
    }
    let bad_url = file_route_encoded(run, "../rec_01.mp4", "%2e%2e%2frec_01.mp4");
    assert_eq!(request(&world.app, Method::GET, &bad_url, &[("range", "bytes=0-3")], "").await.0, StatusCode::NOT_FOUND);
    #[cfg(unix)]
    {
        let recording = world.disk_file(run, name);
        let outside = world.temp.root.join("outside.mp4");
        std::fs::rename(&recording, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, recording).unwrap();
        assert_eq!(request(&world.app, Method::GET, &url, &[("range", "bytes=0-3")], "").await.0, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn run详情按段返回录屏索引和步骤开始时间() {
    let world = World::new().await;
    let run_id = world.run().await;
    let pool = world.state.pool();
    runs::claim_next(pool, world.node).await.unwrap().unwrap();
    runs::mark_started(pool, world.node, run_id).await.unwrap();
    let epoch = 1_700_000_000_000_i64;
    for (idx, at_ms) in [(1, epoch + 500), (2, 0)] {
        let step = Step { run_id, idx, at_ms, tool: "tap".into(), args: json!({}), result: json!({}), file_ref: None };
        assert!(runs::record_step(pool, world.node, &step).await.unwrap());
        assert!(!runs::record_step(pool, world.node, &step).await.unwrap());
    }
    for name in ["rec_02.mp4", "rec_01.mp4", "rec_01.mp4"] {
        assert_eq!(upload(&world.app, run_id, world.node, name, MP4.to_vec()).await.0, StatusCode::NO_CONTENT);
    }
    let segments = json!([
        {"seq": 2, "file": "rec_02.mp4", "start_epoch_ms": epoch + 5000, "duration_ms": 500},
        {"seq": 1, "file": "rec_01.mp4", "start_epoch_ms": epoch, "duration_ms": 4999}
    ]);
    for _ in 0..2 {
        assert_eq!(put_index(&world, run_id, world.node, segments.clone()).await, StatusCode::NO_CONTENT);
    }
    let invalid = json!([{"seq": 1, "file": "rec_02.mp4", "start_epoch_ms": epoch, "duration_ms": 9000}]);
    assert_eq!(put_index(&world, run_id, world.node, invalid).await, StatusCode::BAD_REQUEST);
    let (status, _, body) = request(&world.app, Method::GET, &format!("/api/runs/{run_id}"), &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let detail = json_body(&body);
    assert_eq!(detail["run_steps"][0]["at_ms"], epoch + 500);
    assert!(detail["run_steps"][1]["at_ms"].is_null());
    assert_eq!(detail["files"], json!([]));
    // 详情返回的录屏地址必须是带 exp 和 sig 的签名地址；签名含过期时间不能整串比较，
    // 先逐条确认前缀和签名参数，再去掉签名部分比较其余字段。
    let mut recordings = detail["recordings"].clone();
    for (item, name) in recordings.as_array_mut().unwrap().iter_mut().zip(["rec_01.mp4", "rec_02.mp4"]) {
        let bare = bare_file_route(run_id, name);
        let url = item["url"].as_str().unwrap().to_string();
        assert!(url.starts_with(&format!("{bare}?exp=")) && url.contains("&sig="), "录屏地址缺少签名：{url}");
        item["url"] = json!(bare);
    }
    assert_eq!(recordings, json!([
        {"seq": 1, "url": bare_file_route(run_id, "rec_01.mp4"), "size": MP4.len(), "start_epoch_ms": epoch, "duration_ms": 4999},
        {"seq": 2, "url": bare_file_route(run_id, "rec_02.mp4"), "size": MP4.len(), "start_epoch_ms": epoch + 5000, "duration_ms": 500}
    ]));
}

#[tokio::test]
async fn 录屏留存按时限和容量预览再清理整次执行且保留其他证据() {
    use crate::recording_retention::prune;

    let world = World::new().await;
    let pool = world.state.pool();
    let data = &world.temp.paths.data;
    let expired = world.run().await;
    let older = world.run().await;
    let newer = world.run().await;
    let active = world.run().await;
    let recent = world.run().await;
    for run in [expired, older, newer, active, recent] {
        assert_eq!(upload(&world.app, run, world.node, "rec_01.mp4", MP4.to_vec()).await.0, StatusCode::NO_CONTENT);
    }
    assert_eq!(upload(&world.app, expired, world.node, "rec_02.mp4", MP4.to_vec()).await.0, StatusCode::NO_CONTENT);
    assert_eq!(upload(&world.app, expired, world.node, "final_page.txt", b"evidence".to_vec()).await.0, StatusCode::NO_CONTENT);
    for (run, days) in [(expired, 15), (older, 2), (newer, 1), (recent, 0)] {
        runs::cancel(pool, run).await.unwrap();
        sqlx::query("update runs set finished_at = now() - make_interval(days => $2) where id = $1")
            .bind(run).bind(days).execute(pool).await.unwrap();
    }

    let limit = MP4.len() as u64 * 3;
    let preview = prune(pool, data, 14, limit, false).await.unwrap();
    assert_eq!((preview.runs, preview.segments, preview.remaining_bytes), (2, 3, limit));
    assert!(world.disk_file(expired, "rec_02.mp4").exists());
    let actual = prune(pool, data, 14, limit, true).await.unwrap();
    assert_eq!((actual.runs, actual.segments, actual.removed_bytes, actual.remaining_bytes), (2, 3, limit, limit));
    for (run, name) in [(expired, "rec_01.mp4"), (expired, "rec_02.mp4"), (older, "rec_01.mp4")] {
        assert!(!world.disk_file(run, name).exists());
        assert_eq!(request(&world.app, Method::GET, &file_route(run, name), &[], Body::empty()).await.0, StatusCode::NOT_FOUND);
    }
    let detail = request(&world.app, Method::GET, &format!("/api/runs/{expired}"), &[], Body::empty()).await;
    assert_eq!(json_body(&detail.2)["recordings"], json!([]));
    assert_eq!(tokio::fs::read(world.disk_file(expired, "final_page.txt")).await.unwrap(), b"evidence");
    let protected = prune(pool, data, 14, 1, true).await.unwrap();
    assert_eq!(protected.remaining_bytes, MP4.len() as u64 * 2);
    assert!(world.disk_file(active, "rec_01.mp4").exists());
    assert!(world.disk_file(recent, "rec_01.mp4").exists());
    assert_eq!(prune(pool, data, 14, 1, true).await.unwrap().runs, 0);
}

#[cfg(unix)]
#[tokio::test]
async fn 录屏清理拒绝链接和错误数据目录并保留原登记() {
    use crate::recording_retention::prune;

    let world = World::new().await;
    let pool = world.state.pool();
    let data = &world.temp.paths.data;
    let run = world.run().await;
    assert_eq!(upload(&world.app, run, world.node, "rec_01.mp4", MP4.to_vec()).await.0, StatusCode::NO_CONTENT);
    runs::cancel(pool, run).await.unwrap();
    sqlx::query("update runs set finished_at = now() - interval '2 days' where id = $1").bind(run).execute(pool).await.unwrap();
    assert!(prune(pool, &data.join("missing"), 1, 1, true).await.is_err());

    let path = world.disk_file(run, "rec_01.mp4");
    let outside = world.temp.root.join("outside.mp4");
    std::fs::rename(&path, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    assert!(prune(pool, data, 1, 1, true).await.is_err());
    assert_eq!(std::fs::read(outside).unwrap(), MP4);
    assert!(std::fs::symlink_metadata(path).unwrap().is_symlink());
    assert_eq!(prune(pool, data, 1, 1, false).await.unwrap().segments, 1);
}

#[tokio::test]
async fn 文件上传允许终态十分钟内补传() {
    let world = World::new().await;
    let run = world.run().await;
    runs::cancel(world.state.pool(), run).await.unwrap();
    assert_eq!(
        upload(&world.app, run, world.node, "final_page.txt", Vec::new())
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    sqlx::query("update runs set finished_at = now() - interval '11 minutes' where id = $1")
        .bind(run)
        .execute(world.state.pool())
        .await
        .unwrap();
    let (status, _, body) = upload(&world.app, run, world.node, "01_final.png", PNG.to_vec()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.is_empty());
}

#[tokio::test]
async fn 文件上传落盘失败会回滚登记() {
    let world = World::new().await;
    let run = world.run().await;
    let name = "01_after.png";
    let destination = world.disk_file(run, name);
    std::fs::create_dir_all(&destination).unwrap();
    assert_eq!(
        upload(&world.app, run, world.node, name, PNG.to_vec())
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let count: i64 = sqlx::query_scalar("select count(*) from files where run_id = $1")
        .bind(run)
        .fetch_one(world.state.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        std::fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn run序列化递归脱敏并保留公开字段() {
    let mut value = json!({
        "nested": [{"LeaseId": "测试数据", "PASSWORD": "测试数据", "target": "选择框"}],
        "value": "描述 {{account.password}} 的操作"
    });
    scrub_fields(&mut value);
    assert_eq!(
        value,
        json!({"nested": [{"target": "选择框"}], "value": "描述 （已脱敏） 的操作"})
    );
}

#[tokio::test]
async fn run详情返回有序步骤与逐条证据并过滤凭证字段() {
    let world = World::new().await;
    let run_id = world.run().await;
    let pool = world.state.pool();
    runs::claim_next(pool, world.node).await.unwrap().unwrap();
    runs::mark_started(pool, world.node, run_id).await.unwrap();
    sqlx::query("update runs set case_spec = jsonb_set(case_spec, '{data}', $2) where id = $1")
        .bind(run_id)
        .bind(json!({"password": "{{account.password}}"}))
        .execute(pool)
        .await
        .unwrap();
    for idx in [2, 1] {
        runs::record_step(pool, world.node, &Step {
            run_id, idx, at_ms: 0, tool: "tap".into(),
            args: json!({"password": "模拟旧数据", "target": "选中", "nested": [{"Lease_ID": "模拟旧数据", "value": "{{account.password}}"}]}),
            result: json!({"new_labels": ["已选中"]}), file_ref: Some("shots/01_after.png".into()),
        }).await.unwrap();
    }
    runs::executor_done(
        pool,
        world.node,
        &ExecutorDone {
            run_id,
            result: Verdict::Uncertain,
            reason: "执行员未看到选中".into(),
            cost: Some(json!({"model": "测试执行模型", "effort": "max", "wall_s": 3.0})),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        upload(&world.app, run_id, world.node, "01_after.png", PNG.to_vec())
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    runs::judge_done(
        pool,
        world.node,
        &JudgeDone {
            run_id,
            final_result: FinalResult::Passed,
            judge_status: JudgeStatus::Completed,
            items: vec![JudgeItem {
                idx: 1,
                verdict: Verdict::Passed,
                evidence: "截图可见选中标记".into(),
                file_refs: ["01_after.png", "step:2", "step:", "step:abc", "step:../x", "step:12345"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }],
        },
    )
    .await
    .unwrap();
    let (status, _, bytes) = request(
        &world.app,
        Method::GET,
        &format!("/api/runs/{run_id}"),
        &[],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = text(&bytes).to_ascii_lowercase();
    for forbidden in ["password", "lease", "模拟旧数据"] {
        assert!(!body.contains(forbidden));
    }
    let run = json_body(&bytes);
    assert_eq!(run["case_spec"]["steps"], json!(["点击选择框"]));
    assert_eq!(run["case_spec"]["expected"][0]["text"], "出现选中标记");
    assert!(run["case_spec"].get("data").is_none());
    assert_eq!(run["executor_result"], "uncertain");
    assert_eq!(run["executor_reason"], "执行员未看到选中");
    assert_eq!(run["judge_status"], "completed");
    assert_eq!(run["final_result"], "passed");
    assert_eq!(run["judge_items"][0]["file_refs"], json!(["01_after.png", "step:2"]));
    assert_eq!(run["files"], json!(["01_after.png"]));
    assert_eq!(run["cost"]["effort"], "max");
    assert_eq!(run["run_steps"][0]["idx"], 1);
    assert_eq!(run["run_steps"][1]["idx"], 2);
    assert_eq!(
        run["run_steps"][0]["args"]["nested"][0]["value"],
        "（已脱敏）"
    );
    assert_eq!(run["run_steps"][0]["args"]["target"], "选中");
    assert!(run["run_steps"][0]["ts"].is_string());
}

// ---------------------------------------------------------------- 局域网模式的 Host / Origin 规则（纯函数，不需要数据库）

#[test]
fn 默认模式下私有网段ip也被拒绝() {
    for h in ["192.168.1.5:5173", "10.0.0.2", "172.16.3.4:8080"] {
        assert!(!authority_allowed(h, false), "没开局域网模式，{h} 不该通过");
    }
    assert!(authority_allowed("127.0.0.1:5173", false) && authority_allowed("localhost:5173", false) && authority_allowed("[::1]:5173", false));
}

#[test]
fn 局域网模式只放行私有网段ip字面量() {
    for h in ["192.168.1.5", "192.168.1.5:5173", "10.1.2.3:8080", "172.16.0.1", "172.31.255.254:80", "127.0.0.1:1", "localhost"] {
        assert!(authority_allowed(h, true), "{h} 应通过");
    }
    for h in [
        "8.8.8.8:80", "172.15.0.1", "172.32.0.1", "192.169.1.1", "11.0.0.1", // 非私有网段
        "evil.example.com", "evil.example.com:8080", "mac.local", "192.168.1.5.evil.com", // 域名，含「前缀像内网 IP」的域名
        "0xc0.168.1.5", "192.168.01.5", "192.168.1", "192.168.1.5.", "3232235781", // 会被别的解析器当成内网地址的怪写法
        "192.168.1.5:", "192.168.1.5:80x", "user@192.168.1.5", "192.168.1.256", "",
    ] {
        assert!(!authority_allowed(h, true), "{h:?} 不该通过（DNS 重绑定 / 怪写法）");
    }
}

#[test]
fn 局域网模式下origin同样只认私有网段ip() {
    assert!(allowed_origin("http://192.168.1.5:5173", true));
    assert!(allowed_origin("https://10.0.0.2", true));
    assert!(!allowed_origin("http://192.168.1.5:5173", false));
    assert!(!allowed_origin("http://evil.example.com", true), "域名 Origin 任何时候都不行");
    assert!(!allowed_origin("http://8.8.8.8", true));
    assert!(!allowed_origin("ftp://192.168.1.5", true));
}

