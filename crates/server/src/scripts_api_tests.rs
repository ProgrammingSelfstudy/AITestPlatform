//! 脚本归档 HTTP 接口测试。夹具沿用上一级的 `World`。

use super::*;
use crate::scripts::file_digests;
use std::collections::BTreeMap;

fn script_route(run: Uuid) -> String {
    format!("/api/runs/{run}/scripts")
}

fn node_headers(node: Uuid) -> Vec<(&'static str, String)> {
    vec![("authorization", format!("Bearer {TOKEN}")), ("x-node-id", node.to_string())]
}

/// `request` 要 `&[(&str, &str)]`，这里把拥有所有权的头借出去。
async fn send(app: &Router, method: Method, path: &str, headers: &[(&'static str, String)], body: impl Into<Body>) -> (StatusCode, Bytes) {
    let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (status, _, bytes) = request(app, method, path, &borrowed, body).await;
    (status, bytes)
}

fn files(body: &str) -> BTreeMap<String, String> {
    [("test_tc04.py".to_string(), body.to_string()), ("pages/tc04_pages.py".to_string(), "class Page:\n    pass\n".to_string())].into()
}

fn upload_body(files: &BTreeMap<String, String>) -> Value {
    json!({"files": files, "sha256": file_digests(files), "notes": "测试", "source": "claude_cli", "model": "m", "effort": "high"})
}

/// 把排队中的 run 直接推成「Judge 整体通过」：库里的约束由测试自己保证合法。
async fn make_passed(world: &World, run: Uuid) {
    sqlx::query(
        "update runs set status = 'finished', executor_result = 'passed', judge_status = 'completed', final_result = 'passed',
                started_at = clock_timestamp(), finished_at = clock_timestamp() where id = $1",
    )
    .bind(run)
    .execute(world.state.pool())
    .await
    .unwrap();
    sqlx::query(
        "insert into judge_items (run_id, idx, expected_text, verdict)
         select $1, (e ->> 'idx')::int, e ->> 'text', 'passed' from runs, jsonb_array_elements(case_spec -> 'expected') e where runs.id = $1",
    )
    .bind(run)
    .execute(world.state.pool())
    .await
    .unwrap();
}

async fn case_of(world: &World, run: Uuid) -> Uuid {
    sqlx::query_scalar("select case_id from runs where id = $1").bind(run).fetch_one(world.state.pool()).await.unwrap()
}

#[tokio::test]
async fn 节点上传候选_新建201_重复200_同用例再列出() {
    let world = World::new().await;
    let run = world.run().await;
    make_passed(&world, run).await;
    let headers = node_headers(world.node);

    let first = upload_body(&files("def test_a():\n    assert 1\n"));
    let (status, body) = send(&world.app, Method::PUT, &script_route(run), &headers, first.to_string()).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&body));
    let created = json_body(&body);
    assert_eq!((created["version"].as_i64(), created["status"].as_str(), created["source_run_id"].as_str()), (Some(1), Some("candidate"), Some(run.to_string().as_str())));
    assert!(created.get("files").is_none(), "响应里不能带源码");

    let (status, body) = send(&world.app, Method::PUT, &script_route(run), &headers, first.to_string()).await;
    assert_eq!(status, StatusCode::OK, "重复上传必须幂等：{}", text(&body));
    assert_eq!(json_body(&body)["id"], created["id"], "重复上传指回已有记录");

    let second = upload_body(&files("def test_a():\n    assert 2\n"));
    let (status, body) = send(&world.app, Method::PUT, &script_route(run), &headers, second.to_string()).await;
    assert_eq!((status, json_body(&body)["version"].as_i64()), (StatusCode::CREATED, Some(2)));

    let case = case_of(&world, run).await;
    let (status, body) = send(&world.app, Method::GET, &format!("/api/cases/{case}/scripts"), &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let listed = json_body(&body);
    let versions: Vec<i64> = listed.as_array().unwrap().iter().map(|s| s["version"].as_i64().unwrap()).collect();
    assert_eq!(versions, vec![2, 1], "新的在前");
    assert!(listed.as_array().unwrap().iter().all(|s| s.get("files").is_none()), "列表不带源码");
}

#[tokio::test]
async fn 执行结束很久以后仍可上传_不像证据文件那样限十分钟() {
    let world = World::new().await;
    let run = world.run().await;
    make_passed(&world, run).await;
    sqlx::query("update runs set finished_at = clock_timestamp() - interval '3 days' where id = $1").bind(run).execute(world.state.pool()).await.unwrap();
    let body = upload_body(&files("def test_a():\n    assert 1\n"));
    let (status, _) = send(&world.app, Method::PUT, &script_route(run), &node_headers(world.node), body.to_string()).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn 只能为自己节点的执行归档_未认证拒绝() {
    let world = World::new().await;
    let run = world.run().await;
    make_passed(&world, run).await;
    let body = upload_body(&files("def test_a():\n    assert 1\n")).to_string();

    // 另一个节点：令牌对，但这次执行不是它的。
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, '{\"model_sources\":[]}'::jsonb, true)").bind(other).execute(world.state.pool()).await.unwrap();
    let (status, _) = send(&world.app, Method::PUT, &script_route(run), &node_headers(other), body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "不能为别人节点的执行归档");

    let no_auth = vec![("x-node-id", world.node.to_string())];
    assert_eq!(send(&world.app, Method::PUT, &script_route(run), &no_auth, body.clone()).await.0, StatusCode::UNAUTHORIZED);
    let wrong = vec![("authorization", "Bearer wrong-token".to_string()), ("x-node-id", world.node.to_string())];
    assert_eq!(send(&world.app, Method::PUT, &script_route(run), &wrong, body.clone()).await.0, StatusCode::UNAUTHORIZED);
    let count: i64 = sqlx::query_scalar("select count(*) from scripts").fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(count, 0, "被拒绝的请求不能留下记录");

    let (status, _) = send(&world.app, Method::PUT, "/api/runs/not-a-uuid/scripts", &node_headers(world.node), body.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&world.app, Method::PUT, &script_route(Uuid::new_v4()), &node_headers(world.node), body).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn 来源执行不合格或内容不合规_返回422和中文原因() {
    let world = World::new().await;
    let run = world.run().await; // 还在排队，没有 Judge 结果
    let headers = node_headers(world.node);
    let good = upload_body(&files("def test_a():\n    assert 1\n"));

    let (status, body) = send(&world.app, Method::PUT, &script_route(run), &headers, good.to_string()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(text(&body).contains("Judge"), "要说明原因：{}", text(&body));

    make_passed(&world, run).await;
    let mut tampered = good.clone();
    tampered["sha256"]["test_tc04.py"] = json!("0".repeat(64));
    let (status, body) = send(&world.app, Method::PUT, &script_route(run), &headers, tampered.to_string()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(text(&body).contains("test_tc04.py"), "{}", text(&body));

    let mut evil = files("x = 1\n");
    evil.insert("../evil.py".into(), "x = 1\n".into());
    let (status, body) = send(&world.app, Method::PUT, &script_route(run), &headers, upload_body(&evil).to_string()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(text(&body).contains("不接收"), "{}", text(&body));

    let mut bad_source = good.clone();
    bad_source["source"] = json!("gpt_api_key");
    assert_eq!(send(&world.app, Method::PUT, &script_route(run), &headers, bad_source.to_string()).await.0, StatusCode::UNPROCESSABLE_ENTITY, "不认识的模型来源");

    let count: i64 = sqlx::query_scalar("select count(*) from scripts").fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(count, 0, "所有被拒绝的请求都不能留下记录");
}

#[tokio::test]
async fn 请求格式错误或过大_拒绝() {
    let world = World::new().await;
    let run = world.run().await;
    make_passed(&world, run).await;
    let headers = node_headers(world.node);
    assert_eq!(send(&world.app, Method::PUT, &script_route(run), &headers, "{").await.0, StatusCode::BAD_REQUEST, "不是合法 JSON");
    let mut unknown = upload_body(&files("x = 1\n"));
    unknown["extra"] = json!(1);
    assert_eq!(send(&world.app, Method::PUT, &script_route(run), &headers, unknown.to_string()).await.0, StatusCode::BAD_REQUEST, "未知字段必须拒绝，不能悄悄忽略");
    let huge = json!({"files": {"test_tc04.py": "x".repeat(600 * 1024)}, "sha256": {}, "source": "claude_cli"});
    assert_eq!(send(&world.app, Method::PUT, &script_route(run), &headers, huge.to_string()).await.0, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn 列表接口对不存在的用例返回404() {
    let world = World::new().await;
    assert_eq!(send(&world.app, Method::GET, &format!("/api/cases/{}/scripts", Uuid::new_v4()), &[], "").await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&world.app, Method::GET, "/api/cases/not-a-uuid/scripts", &[], "").await.0, StatusCode::NOT_FOUND);
}

fn source_route(run: Uuid) -> String {
    format!("/api/runs/{run}/script-source")
}

#[tokio::test]
async fn 节点取脚本来源_带完整用例和逐条判定_页面接口没有的字段也在() {
    let world = World::new().await;
    let run = world.run().await;
    make_passed(&world, run).await;

    // 对照：页面用的公开详情故意不返回用例 data 和前置条件。
    let (_, _, public) = request(&world.app, Method::GET, &format!("/api/runs/{run}"), &[], "").await;
    assert!(json_body(&public)["case_spec"].get("data").is_none(), "前提：公开接口不外露用例 data");

    let (status, body) = send(&world.app, Method::GET, &source_route(run), &node_headers(world.node), "").await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let source = json_body(&body);
    assert_eq!(source["run_id"], json!(run.to_string()));
    assert_eq!((source["final_result"].as_str(), source["judge_status"].as_str()), (Some("passed"), Some("completed")));
    for key in ["code", "title", "preconditions", "steps", "expected", "data"] {
        assert!(source["case_spec"].get(key).is_some(), "完整用例必须带 {key}");
    }
    let expected = source["case_spec"]["expected"].as_array().unwrap().len();
    let items = source["judge_items"].as_array().unwrap();
    assert_eq!(items.len(), expected, "逐条判定和预期条数一致");
    assert!(items.iter().all(|i| i["verdict"] == "passed" && i.get("evidence").is_some() && i.get("file_refs").is_some()));
    assert!(source["judge"]["source"].is_string(), "要带这次任务里 Judge 的角色配置，生成角色据此解析：{source}");
    assert!(source["case_version"].as_i64().is_some() && source["case_id"].is_string());
}

#[tokio::test]
async fn 取脚本来源只能取自己节点的执行_未认证拒绝() {
    let world = World::new().await;
    let run = world.run().await;
    make_passed(&world, run).await;

    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, '{\"model_sources\":[]}'::jsonb, true)").bind(other).execute(world.state.pool()).await.unwrap();
    assert_eq!(send(&world.app, Method::GET, &source_route(run), &node_headers(other), "").await.0, StatusCode::FORBIDDEN, "不能读别人节点的执行");
    assert_eq!(send(&world.app, Method::GET, &source_route(run), &[], "").await.0, StatusCode::UNAUTHORIZED);
    let wrong = vec![("authorization", "Bearer wrong-token".to_string()), ("x-node-id", world.node.to_string())];
    assert_eq!(send(&world.app, Method::GET, &source_route(run), &wrong, "").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&world.app, Method::GET, &source_route(Uuid::new_v4()), &node_headers(world.node), "").await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&world.app, Method::GET, "/api/runs/not-a-uuid/script-source", &node_headers(world.node), "").await.0, StatusCode::NOT_FOUND);
}

/// 上传一条候选，返回脚本编号。
async fn archived(world: &World, run: Uuid) -> Uuid {
    make_passed(world, run).await;
    let body = upload_body(&files("def test_a():\n    assert 1\n"));
    let (status, bytes) = send(&world.app, Method::PUT, &script_route(run), &node_headers(world.node), body.to_string()).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    json_body(&bytes)["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn 节点取自己的脚本源码_带用例编号和文件() {
    let world = World::new().await;
    let run = world.run().await;
    let script = archived(&world, run).await;
    let (status, body) = send(&world.app, Method::GET, &format!("/api/scripts/{script}/files"), &node_headers(world.node), "").await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let got = json_body(&body);
    assert_eq!(got["status"], "candidate");
    assert_eq!(got["files"]["test_tc04.py"], "def test_a():\n    assert 1\n");
    assert!(got["case_code"].as_str().is_some_and(|c| !c.is_empty()));
}

#[tokio::test]
async fn 自检结果只能由来源节点回报_未认证拒绝() {
    let world = World::new().await;
    let run = world.run().await;
    let script = archived(&world, run).await;
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, '{\"model_sources\":[]}'::jsonb, true)").bind(other).execute(world.state.pool()).await.unwrap();
    let report = json!({"passed": true, "detail": "x"}).to_string();
    for path in [format!("/api/scripts/{script}/files"), format!("/api/scripts/{script}/self-check")] {
        let method = if path.ends_with("files") { Method::GET } else { Method::PUT };
        let (status, _) = send(&world.app, method.clone(), &path, &node_headers(other), report.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "别的节点不能碰：{path}");
        let no_auth = vec![("x-node-id", world.node.to_string())];
        assert_eq!(send(&world.app, method, &path, &no_auth, report.clone()).await.0, StatusCode::UNAUTHORIZED);
    }
    let status: String = sqlx::query_scalar("select status from scripts where id = $1").bind(script).fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(status, "candidate", "被拒绝的回报不能改状态");
    let (status, _) = send(&world.app, Method::PUT, &format!("/api/scripts/{}/self-check", Uuid::new_v4()), &node_headers(world.node), report).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn 自检通过变自检通过_失败变已拒绝并记下原始输出_不能重复回报() {
    let world = World::new().await;
    let run = world.run().await;
    let script = archived(&world, run).await;
    let route = format!("/api/scripts/{script}/self-check");
    let headers = node_headers(world.node);

    let (status, body) = send(&world.app, Method::PUT, &route, &headers, json!({"passed": false, "detail": "AssertionError: 目标歌曲没找到"}).to_string()).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let got = json_body(&body);
    assert_eq!((got["status"].as_str(), got["status_note"].as_str()), (Some("rejected"), Some("AssertionError: 目标歌曲没找到")));

    let (status, _) = send(&world.app, Method::PUT, &route, &headers, json!({"passed": true, "detail": "ok"}).to_string()).await;
    assert_eq!(status, StatusCode::CONFLICT, "已拒绝的脚本不能再回报通过");

    // 通过的路径：再来一个版本。
    let second = upload_body(&files("def test_a():\n    assert 2\n"));
    let (_, bytes) = send(&world.app, Method::PUT, &script_route(run), &headers, second.to_string()).await;
    let next: Uuid = json_body(&bytes)["id"].as_str().unwrap().parse().unwrap();
    let (status, body) = send(&world.app, Method::PUT, &format!("/api/scripts/{next}/self-check"), &headers, json!({"passed": true, "detail": "1 passed"}).to_string()).await;
    assert_eq!((status, json_body(&body)["status"].as_str()), (StatusCode::OK, Some("self_check_passed")));
    let (status, _) = send(&world.app, Method::PUT, &format!("/api/scripts/{next}/self-check"), &headers, "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------- 人工获准、下线、拒绝

/// 人工操作：审核令牌 + 浏览器写保护要求的头。`token` 为 None 就不带令牌。
async fn human(world: &World, script: Uuid, action: &str, note: Option<&str>, token: Option<&str>) -> (StatusCode, Value) {
    let mut headers = WRITE_HEADERS.to_vec();
    if let Some(token) = token {
        headers.push(("x-review-token", token));
    }
    let body = note.map_or_else(|| json!({}), |n| json!({"note": n}));
    let (status, _, bytes) = request(&world.app, Method::POST, &format!("/api/scripts/{script}/{action}"), &headers, body.to_string()).await;
    (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(text(&bytes))))
}

async fn self_check_passed(world: &World, script: Uuid, detail: &str) {
    let (status, _) = send(&world.app, Method::PUT, &format!("/api/scripts/{script}/self-check"), &node_headers(world.node), json!({"passed": true, "detail": detail}).to_string()).await;
    assert_eq!(status, StatusCode::OK);
}

/// 同一次通过的执行再归档一条内容不同的候选。
async fn archive_another(world: &World, run: Uuid, marker: &str) -> Uuid {
    let body = upload_body(&files(&format!("def test_a():\n    assert {marker}\n")));
    let (status, bytes) = send(&world.app, Method::PUT, &script_route(run), &node_headers(world.node), body.to_string()).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&bytes));
    json_body(&bytes)["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn 完整流程_不能跳过自检_获准保留自检输出_下线要写原因() {
    let world = World::new().await;
    let run = world.run().await;
    let script = archived(&world, run).await;

    let (status, body) = human(&world, script, "approve", None, Some(REVIEW_TOKEN)).await;
    assert_eq!(status, StatusCode::CONFLICT, "候选不能跳过自检直接获准");
    assert!(body.as_str().unwrap().contains("candidate"), "{body}");

    self_check_passed(&world, script, "1 passed in 70s").await;
    let (status, body) = human(&world, script, "approve", Some("看过轨迹，可以用"), Some(REVIEW_TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "approved");
    let note = body["status_note"].as_str().unwrap();
    assert!(note.contains("1 passed in 70s") && note.contains("获准：看过轨迹，可以用"), "自检输出要保留，后面追加人的决定：{note}");

    let (status, _) = human(&world, script, "retire", None, Some(REVIEW_TOKEN)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "下线要写原因");
    let (status, _) = human(&world, script, "retire", Some("  "), Some(REVIEW_TOKEN)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "只有空白也不算原因");
    let (status, body) = human(&world, script, "retire", Some("界面改版了"), Some(REVIEW_TOKEN)).await;
    assert_eq!((status, body["status"].as_str()), (StatusCode::OK, Some("retired")));
    assert!(body["status_note"].as_str().unwrap().contains("下线：界面改版了"));

    let (status, _) = human(&world, script, "retire", Some("再来一次"), Some(REVIEW_TOKEN)).await;
    assert_eq!(status, StatusCode::CONFLICT, "下线是终点");
    let (status, _) = human(&world, script, "approve", None, Some(REVIEW_TOKEN)).await;
    assert_eq!(status, StatusCode::CONFLICT, "下线后不能再获准");
}

#[tokio::test]
async fn 同一用例只能有一个获准版本_先下线才能获准新版本() {
    let world = World::new().await;
    let run = world.run().await;
    let first = archived(&world, run).await;
    let second = archive_another(&world, run, "2").await;
    self_check_passed(&world, first, "ok").await;
    self_check_passed(&world, second, "ok").await;

    assert_eq!(human(&world, first, "approve", None, Some(REVIEW_TOKEN)).await.0, StatusCode::OK);
    let (status, body) = human(&world, second, "approve", None, Some(REVIEW_TOKEN)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.as_str().unwrap().contains("已有获准的版本 1"), "{body}");
    let still: String = sqlx::query_scalar("select status from scripts where id = $1").bind(second).fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(still, "self_check_passed", "失败的获准不能改状态");

    assert_eq!(human(&world, first, "retire", Some("换新版"), Some(REVIEW_TOKEN)).await.0, StatusCode::OK);
    assert_eq!(human(&world, second, "approve", None, Some(REVIEW_TOKEN)).await.0, StatusCode::OK);
}

#[tokio::test]
async fn 用例内容变过的旧脚本不能获准() {
    let world = World::new().await;
    let run = world.run().await;
    let script = archived(&world, run).await;
    self_check_passed(&world, script, "ok").await;
    sqlx::query("update test_cases set version = version + 1 where id = $1").bind(case_of(&world, run).await).execute(world.state.pool()).await.unwrap();
    let (status, body) = human(&world, script, "approve", None, Some(REVIEW_TOKEN)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.as_str().unwrap().contains("用例内容变过"), "{body}");
}

#[tokio::test]
async fn 人工拒绝要写原因_候选和自检通过都可拒绝_获准后不能拒绝() {
    let world = World::new().await;
    let run = world.run().await;
    let a = archived(&world, run).await;
    let b = archive_another(&world, run, "2").await;
    let c = archive_another(&world, run, "3").await;
    assert_eq!(human(&world, a, "reject", None, Some(REVIEW_TOKEN)).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = human(&world, a, "reject", Some("断言太弱"), Some(REVIEW_TOKEN)).await;
    assert_eq!((status, body["status"].as_str()), (StatusCode::OK, Some("rejected")));
    self_check_passed(&world, b, "ok").await;
    assert_eq!(human(&world, b, "reject", Some("不想用"), Some(REVIEW_TOKEN)).await.0, StatusCode::OK);
    self_check_passed(&world, c, "ok").await;
    human(&world, c, "approve", None, Some(REVIEW_TOKEN)).await;
    assert_eq!(human(&world, c, "reject", Some("反悔"), Some(REVIEW_TOKEN)).await.0, StatusCode::CONFLICT, "获准后只能下线");
}

#[tokio::test]
async fn 人工操作要审核令牌_节点令牌不行_未知脚本404() {
    let world = World::new().await;
    let run = world.run().await;
    let script = archived(&world, run).await;
    self_check_passed(&world, script, "ok").await;
    for token in [None, Some("wrong-token-0123456789")] {
        for action in ["approve", "retire", "reject"] {
            assert_eq!(human(&world, script, action, Some("x"), token).await.0, StatusCode::FORBIDDEN, "{action} {token:?}");
        }
    }
    // 节点令牌和节点编号不是人的授权。
    let mut headers = WRITE_HEADERS.to_vec();
    let bearer = format!("Bearer {TOKEN}");
    let node = world.node.to_string();
    headers.extend([("authorization", bearer.as_str()), ("x-node-id", node.as_str())]);
    let (status, _, _) = request(&world.app, Method::POST, &format!("/api/scripts/{script}/approve"), &headers, "{}").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "节点不能替人决定");
    let still: String = sqlx::query_scalar("select status from scripts where id = $1").bind(script).fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(still, "self_check_passed", "被拒绝的请求不能改状态");

    assert_eq!(human(&world, Uuid::new_v4(), "approve", None, Some(REVIEW_TOKEN)).await.0, StatusCode::NOT_FOUND);
    let (status, _, _) = request(&world.app, Method::POST, "/api/scripts/not-a-uuid/approve", &review_headers(REVIEW_TOKEN), "{}").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn 状态说明不超过库里的上限_自检输出长也能获准() {
    let world = World::new().await;
    let run = world.run().await;
    let script = archived(&world, run).await;
    let long = "长".repeat(1800);
    self_check_passed(&world, script, &long).await;
    let reason = "因".repeat(300);
    let (status, body) = human(&world, script, "approve", Some(&reason), Some(REVIEW_TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let note = body["status_note"].as_str().unwrap();
    assert!(note.chars().count() <= 2000 && note.ends_with(&reason), "原因必须完整保留，截的是旧输出的开头");
    assert_eq!(human(&world, script, "retire", Some(&"因".repeat(301)), Some(REVIEW_TOKEN)).await.0, StatusCode::UNPROCESSABLE_ENTITY);

    let other = archive_another(&world, run, "2").await;
    let too_long = "x".repeat(1801);
    let (status, _) = send(&world.app, Method::PUT, &format!("/api/scripts/{other}/self-check"), &node_headers(world.node), json!({"passed": true, "detail": too_long}).to_string()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "自检输出超过 1800 字要拒收，而不是写库时才报错");
}

#[tokio::test]
async fn 按项目列出所有脚本_带用例编号和标题_不带源码() {
    let world = World::new().await;
    let run = world.run().await;
    let first = archived(&world, run).await;
    archive_another(&world, run, "2").await;
    let project: String = sqlx::query_scalar("select p.name from projects p join test_cases c on c.project_id = p.id where c.id = $1").bind(case_of(&world, run).await).fetch_one(world.state.pool()).await.unwrap();
    let (status, body) = send(&world.app, Method::GET, &format!("/api/projects/{project}/scripts"), &[], "").await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let rows = json_body(&body);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["case_code"], "TC-04");
    assert!(rows[0]["case_title"].as_str().is_some_and(|t| !t.is_empty()));
    assert_eq!((rows[0]["version"].as_i64(), rows[1]["version"].as_i64()), (Some(2), Some(1)), "同一用例新版本在前");
    assert_eq!(rows[1]["id"].as_str(), Some(first.to_string().as_str()));
    assert!(rows.iter().all(|r| r.get("files").is_none()), "不带源码");
    let (status, body) = send(&world.app, Method::GET, "/api/projects/不存在的项目/scripts", &[], "").await;
    assert_eq!((status, json_body(&body)), (StatusCode::OK, json!([])));
}

// ---------------------------------------------------------------- 回归

async fn project_of(world: &World, run: Uuid) -> String {
    sqlx::query_scalar("select p.name from projects p join test_cases c on c.project_id = p.id where c.id = $1").bind(case_of(world, run).await).fetch_one(world.state.pool()).await.unwrap()
}

/// 归档、自检通过并获准一条脚本，返回编号。
async fn approved_script(world: &World, run: Uuid) -> Uuid {
    let script = archived(world, run).await;
    self_check_passed(world, script, "ok").await;
    assert_eq!(human(world, script, "approve", None, Some(REVIEW_TOKEN)).await.0, StatusCode::OK);
    script
}

fn regression_body(passed: bool, detail: &str) -> String {
    json!({"serial": "DEV0", "passed": passed, "detail": detail, "duration_ms": 1234}).to_string()
}

#[tokio::test]
async fn 节点取已获准的脚本_没有获准版本是404_要节点认证() {
    let world = World::new().await;
    let run = world.run().await;
    let project = project_of(&world, run).await;
    let route = format!("/api/projects/{project}/cases/TC-04/approved-script");

    let candidate = archived(&world, run).await;
    let (status, _) = send(&world.app, Method::GET, &route, &node_headers(world.node), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "候选不能被拿去回归");
    self_check_passed(&world, candidate, "ok").await;
    let (status, _) = send(&world.app, Method::GET, &route, &node_headers(world.node), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "自检通过但没获准也不行");

    human(&world, candidate, "approve", None, Some(REVIEW_TOKEN)).await;
    let (status, body) = send(&world.app, Method::GET, &route, &node_headers(world.node), "").await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let got = json_body(&body);
    assert_eq!((got["id"].as_str(), got["version"].as_i64()), (Some(candidate.to_string().as_str()), Some(1)));
    assert_eq!(got["files"]["test_tc04.py"], "def test_a():\n    assert 1\n");

    assert_eq!(send(&world.app, Method::GET, &route, &[], "").await.0, StatusCode::UNAUTHORIZED);
    human(&world, candidate, "retire", Some("换新版"), Some(REVIEW_TOKEN)).await;
    assert_eq!(send(&world.app, Method::GET, &route, &node_headers(world.node), "").await.0, StatusCode::NOT_FOUND, "下线后不能再回归");
    let (status, _) = send(&world.app, Method::GET, &format!("/api/projects/{project}/cases/TC-99/approved-script"), &node_headers(world.node), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn 回归结果只能记在已获准的版本上_页面行带最近一次() {
    let world = World::new().await;
    let run = world.run().await;
    let project = project_of(&world, run).await;
    let script = archived(&world, run).await;
    let route = format!("/api/scripts/{script}/regressions");
    let headers = node_headers(world.node);

    let (status, body) = send(&world.app, Method::PUT, &route, &headers, regression_body(true, "1 passed")).await;
    assert_eq!(status, StatusCode::CONFLICT, "候选不能记回归");
    assert!(text(&body).contains("只有已获准"), "{}", text(&body));

    self_check_passed(&world, script, "ok").await;
    human(&world, script, "approve", None, Some(REVIEW_TOKEN)).await;
    assert_eq!(send(&world.app, Method::PUT, &route, &headers, regression_body(true, "1 passed in 70s")).await.0, StatusCode::CREATED);
    let (status, _) = send(&world.app, Method::PUT, &route, &headers, regression_body(false, "AssertionError: 回归了")).await;
    assert_eq!(status, StatusCode::CREATED, "失败也要如实记下");

    let (_, body) = send(&world.app, Method::GET, &format!("/api/projects/{project}/scripts"), &[], "").await;
    let last = &json_body(&body)[0]["last_regression"];
    assert_eq!((last["passed"].as_bool(), last["serial"].as_str(), last["duration_ms"].as_i64()), (Some(false), Some("DEV0"), Some(1234)), "页面显示最近一次：{last}");
    assert!(last["detail"].as_str().unwrap().contains("回归了"));

    // 没跑过回归的版本是 null
    let other = archive_another(&world, run, "2").await;
    let (_, body) = send(&world.app, Method::GET, &format!("/api/projects/{project}/scripts"), &[], "").await;
    let rows = json_body(&body);
    let row = rows.as_array().unwrap().iter().find(|r| r["id"].as_str() == Some(other.to_string().as_str())).unwrap();
    assert!(row["last_regression"].is_null());
}

#[tokio::test]
async fn 回归回报的格式和认证() {
    let world = World::new().await;
    let run = world.run().await;
    let script = approved_script(&world, run).await;
    let route = format!("/api/scripts/{script}/regressions");
    let headers = node_headers(world.node);

    assert_eq!(send(&world.app, Method::PUT, &route, &[], regression_body(true, "x")).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&world.app, Method::PUT, &route, &headers, "not json").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(send(&world.app, Method::PUT, &route, &headers, json!({"serial": "", "passed": true, "detail": "", "duration_ms": 1}).to_string()).await.0, StatusCode::UNPROCESSABLE_ENTITY, "序列号不能为空");
    assert_eq!(send(&world.app, Method::PUT, &route, &headers, regression_body(false, &"x".repeat(1801))).await.0, StatusCode::UNPROCESSABLE_ENTITY, "输出超限要拒收，而不是写库时才报错");
    assert_eq!(send(&world.app, Method::PUT, &route, &headers, json!({"serial": "D", "passed": true, "detail": "", "duration_ms": 1, "extra": 1}).to_string()).await.0, StatusCode::BAD_REQUEST, "未知字段拒绝");
    assert_eq!(send(&world.app, Method::PUT, &format!("/api/scripts/{}/regressions", Uuid::new_v4()), &headers, regression_body(true, "x")).await.0, StatusCode::NOT_FOUND);
    let count: i64 = sqlx::query_scalar("select count(*) from script_regressions").fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(count, 0, "被拒绝的回报不能留下记录");

    // 获准的脚本是项目资产：别的节点也可以拿去在自己的设备上回归，结果记在它名下。
    let other = Uuid::new_v4();
    sqlx::query("insert into nodes (id, caps, online) values ($1, '{\"model_sources\":[]}'::jsonb, true)").bind(other).execute(world.state.pool()).await.unwrap();
    assert_eq!(send(&world.app, Method::PUT, &route, &node_headers(other), regression_body(true, "ok")).await.0, StatusCode::CREATED);
    let owner: Uuid = sqlx::query_scalar("select node_id from script_regressions").fetch_one(world.state.pool()).await.unwrap();
    assert_eq!(owner, other);
}
