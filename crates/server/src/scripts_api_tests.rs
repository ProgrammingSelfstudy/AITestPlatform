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
