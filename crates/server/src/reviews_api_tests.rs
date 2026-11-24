use super::*;
use crate::accounts::{self, AccountKey, NewAccount};
use crate::test_support::test_state_with_accounts;

#[tokio::test]
async fn 未配置或短审核口令始终返回空的403() {
    let temp = TempPaths::new();
    for token in [None, Some("123")] {
        let app = temp.app(with_token(lazy_state(), token));
        for _ in 0..12 {
            let (status, _, body) = request(
                &app,
                Method::POST,
                REVIEW_ROUTE,
                &review_headers("123"),
                "{",
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(body.is_empty());
        }
    }
}

#[tokio::test]
async fn 审核无口令或口令错误同为403空体且第十一次错误后返回429() {
    let temp = TempPaths::new();
    let app = temp.app(with_token(lazy_state(), Some(REVIEW_TOKEN)));
    let (status, _, body) = request(&app, Method::POST, REVIEW_ROUTE, &WRITE_HEADERS, "{").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.is_empty());
    let mut duplicates = review_headers(REVIEW_TOKEN);
    duplicates.push(("x-review-token", REVIEW_TOKEN));
    for headers in std::iter::once(duplicates).chain((0..9).map(|_| review_headers("wrong-token")))
    {
        let (status, _, body) = request(&app, Method::POST, REVIEW_ROUTE, &headers, "{").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.is_empty());
    }
    for headers in [
        review_headers("wrong-token"),
        review_headers(REVIEW_TOKEN),
        WRITE_HEADERS.to_vec(),
    ] {
        let (status, _, body) = request(&app, Method::POST, REVIEW_ROUTE, &headers, "{").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.is_empty());
    }
}

#[tokio::test]
async fn 正确审核口令不能绕过现有浏览器写守卫() {
    let temp = TempPaths::new();
    let state = with_token(lazy_state(), Some(REVIEW_TOKEN));
    let app = app_with_paths(state, temp.paths.clone(), true);
    for (index, wrong) in [
        (0, "example.invalid"),
        (1, "https://example.invalid"),
        (2, "XMLHttpRequest"),
        (3, "text/plain"),
    ] {
        let mut headers = review_headers(REVIEW_TOKEN);
        headers[index].1 = wrong;
        let (status, _, body) = request(&app, Method::POST, REVIEW_ROUTE, &headers, "{").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.is_empty());
    }
    let mut headers = review_headers(REVIEW_TOKEN);
    headers[0].1 = "192.168.1.10:5173";
    headers[1].1 = "http://192.168.1.10:5173";
    let (status, _, body) = request(&app, Method::POST, REVIEW_ROUTE, &headers, "{").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(text(&body), "审核参数格式无效");
}

async fn review_world(key: Option<AccountKey>) -> World {
    let state = test_state_with_accounts(config(), key, "debug").await;
    World::with_state(with_token(state, Some(REVIEW_TOKEN))).await
}

async fn submit_review(world: &World, run: Uuid, body: Value) -> (StatusCode, HeaderMap, Bytes) {
    request(
        &world.app,
        Method::POST,
        &format!("/api/runs/{run}/reviews"),
        &review_headers(REVIEW_TOKEN),
        body.to_string(),
    )
    .await
}

async fn start(world: &World, run: Uuid) {
    let pool = world.state.pool();
    runs::claim_next(pool, world.node).await.unwrap().unwrap();
    runs::mark_started(pool, world.node, run).await.unwrap();
}

async fn finish(world: &World, run: Uuid, item: JudgeItem) {
    let pool = world.state.pool();
    runs::executor_done(
        pool,
        world.node,
        &ExecutorDone {
            run_id: run,
            result: Verdict::Passed,
            reason: "执行完成".into(),
            cost: None,
        },
    )
    .await
    .unwrap();
    runs::judge_done(
        pool,
        world.node,
        &JudgeDone {
            run_id: run,
            final_result: FinalResult::Uncertain,
            judge_status: JudgeStatus::Completed,
            items: vec![item],
        },
    )
    .await
    .unwrap();
}

fn sample_item() -> JudgeItem {
    JudgeItem {
        idx: 1,
        verdict: Verdict::Passed,
        evidence: "选中状态可见".into(),
        file_refs: vec![],
    }
}

#[tokio::test]
async fn 审核拒绝未结束的执行和不存在的判定条目() {
    let world = review_world(None).await;
    let run = world.run().await;
    let body = json!({"mark":"correct","reviewer":"测试员"});
    assert_eq!(
        submit_review(&world, run, body.clone()).await.0,
        StatusCode::CONFLICT
    );
    start(&world, run).await;
    finish(&world, run, sample_item()).await;
    for idx in [2, 0, -1] {
        let mut missing = body.clone();
        missing["item_idx"] = json!(idx);
        assert_eq!(
            submit_review(&world, run, missing).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let count: i64 = sqlx::query_scalar("select count(*) from reviews")
        .fetch_one(world.state.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn 审核成功保留历史和服务端快照且无口令可读历史与最新统计() {
    let world = review_world(None).await;
    let run = world.run().await;
    start(&world, run).await;
    finish(&world, run, sample_item()).await;
    let mut saved = Vec::new();
    for (idx, mark, snapshot) in [
        (Some(1), "correct", "passed"),
        (Some(1), "failed", "passed"),
        (None, "correct", "uncertain"),
    ] {
        let (status, _, body) = submit_review(
            &world,
            run,
            json!({
                "item_idx":idx,"mark":mark,"reviewer":"测试员","judge_verdict":"failed",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{}", text(&body));
        let row = json_body(&body);
        assert_eq!(row["judge_verdict"], snapshot);
        saved.push(row["id"].clone());
    }
    let (status, _, body) = request(
        &world.app,
        Method::GET,
        &format!("/api/runs/{run}/reviews"),
        &[],
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = json_body(&body);
    assert_eq!(rows["history"].as_array().unwrap().len(), 3);
    assert_eq!(rows["history"][2]["id"], saved[0]);
    assert_eq!(rows["history"][2]["judge_verdict"], "passed");
    assert_eq!(rows["current"].as_array().unwrap().len(), 2);
    assert_eq!(rows["current"][0]["id"], saved[2]);
    assert!(rows["current"][0]["item_idx"].is_null());
    assert_eq!(rows["current"][1]["id"], saved[1]);
    assert_eq!(rows["current"][1]["mark"], "failed");
    let (status, _, body) = request(&world.app, Method::GET, "/api/reviews/stats", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    let stats = json_body(&body);
    assert_eq!(stats["reviewed"], 2);
    assert_eq!(stats["judge_correct_rate"], 0.5);
    assert_eq!(stats["confusion"]["passed"]["failed"], 1);
    assert_eq!(stats["confusion"]["uncertain"]["uncertain"], 1);
    assert_eq!(stats["confusion"]["passed"]["passed"], 0);
    assert_eq!(stats["by_case"]["TC-04"]["judge_correct_rate"], 0.5);
}

#[tokio::test]
async fn 无口令下载当前审核且导出不含凭证设备号和路径并带录屏时间() {
    let fake_key = || AccountKey::from_hex(&"2a".repeat(32)).unwrap();
    let world = review_world(Some(fake_key())).await;
    let pool = world.state.pool();
    let username = "sample-reviewer@example.test";
    let password = "synthetic-account-password";
    let serial = "测试设备";
    accounts::import_accounts(
        pool,
        &fake_key(),
        "hafa",
        "debug",
        &[NewAccount {
            username: username.into(),
            password: password.into(),
            nickname: "测试账号昵称".into(),
            tags: vec![],
            device_serial: Some(serial.into()),
        }],
    )
    .await
    .unwrap();
    let run = world.run().await;
    start(&world, run).await;
    let epoch = 1_700_000_000_000_i64;
    assert!(
        runs::record_step(
            pool,
            world.node,
            &Step {
                run_id: run,
                idx: 1,
                at_ms: epoch + 750,
                tool: "tap".into(),
                args: json!({}),
                result: json!({}),
                file_ref: Some("shots/01_after.png".into()),
            }
        )
        .await
        .unwrap()
    );
    assert_eq!(
        upload(&world.app, run, world.node, "01_after.png", PNG.to_vec())
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        upload(&world.app, run, world.node, "rec_01.mp4", MP4.to_vec())
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        put_index(
            &world,
            run,
            world.node,
            json!([
                {"seq":1,"file":"rec_01.mp4","start_epoch_ms":epoch,"duration_ms":2000},
            ])
        )
        .await,
        StatusCode::NO_CONTENT
    );
    finish(
        &world,
        run,
        JudgeItem {
            evidence: format!("截图证明选中；{username} {password} {serial} /private/shot.png"),
            file_refs: ["shots/01_after.png", "/private/shot.png", "step:1"]
                .map(str::to_owned)
                .into(),
            ..sample_item()
        },
    )
    .await;
    let (status, _, _) = submit_review(
        &world,
        run,
        json!({"item_idx":1,"mark":"failed","reviewer":"先前审核员"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, body) = submit_review(&world, run, json!({
        "item_idx":1,"mark":"correct","reviewer":"测试员", "note":format!("请检查\n{REVIEW_TOKEN} {password}"),
    })).await;
    assert_eq!(status, StatusCode::CREATED, "{}", text(&body));
    let saved = json_body(&body);
    let note: String = sqlx::query_scalar("select note from reviews where id = $1")
        .bind(serde_json::from_value::<Uuid>(saved["id"].clone()).unwrap())
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(!note.contains(REVIEW_TOKEN) && !note.contains(password));
    let (status, headers, body) =
        request(&world.app, Method::GET, "/api/reviews/export", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/x-ndjson");
    assert!(
        headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    let output = text(&body);
    for forbidden in [
        username,
        password,
        serial,
        REVIEW_TOKEN,
        "shots/",
        "/private/",
        "先前审核员",
    ] {
        assert!(
            !output.contains(forbidden),
            "下载内容不应含敏感片段或旧标注"
        );
    }
    assert_eq!(output.lines().count(), 1);
    let line = json_body(&body);
    assert_eq!(line["case_code"], "TC-04");
    assert_eq!(line["case_version"], 1);
    assert_eq!(line["expected_text"], "出现选中标记");
    assert_eq!(line["file_refs"], json!(["01_after.png"]));
    assert_eq!(line["judge_verdict"], "passed");
    assert_eq!(line["human_verdict"], "passed");
    assert_eq!(line["has_recording"], true);
    assert_eq!(
        line["recording_position"],
        json!({"step_idx":1,"seq":1,"offset_seconds":0.75})
    );
}
