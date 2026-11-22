use super::*;
use crate::recorder::tests::{FAKE_MP4, FakeAdb};
use crate::runner::Runner;
use crate::uploader::read_trace;
use agent::codex_exec::events::Summary;
use protocol::{CaseExpected, CaseSpec, JudgeDone, Limits, NodeMsg, RoleModel};
use std::collections::HashSet;
use std::convert::Infallible;
use std::fs;
use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};
use axum::{Router, ServiceExt, body::{Body, Bytes, to_bytes}, extract::Path as RoutePath, http::{HeaderMap, Request, StatusCode, header}, routing::{get, put}};
use tokio::sync::{Semaphore, mpsc};
use uuid::Uuid;

struct FinishedExecution {
    real: RealExecutor,
    judge_opts: ClaudeOpts,
}

impl RunExecutor for FinishedExecution {
    async fn execute(&self, assign: RunAssign, rep: Reporter) {
        let dir = self.real.0.runs_dir.join(assign.run_id.to_string());
        self.real
            .report_execution(
                &assign,
                rep,
                &dir,
                Execution {
                    summary: Summary::default(),
                    trace: read_trace(&dir).await,
                    uploaded: HashSet::from(["01_final.png".into()]),
                },
                self.judge_opts.clone(),
            )
            .await;
    }
}

struct Fixture {
    dir: PathBuf,
    run_dir: PathBuf,
    assign: RunAssign,
}

impl Fixture {
    fn new() -> Self {
        let run_id = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("hafa-node-judge-{run_id}"));
        let run_dir = dir.join(run_id.to_string());
        fs::create_dir_all(run_dir.join("shots")).unwrap();
        fs::write(run_dir.join("shots/01_final.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        fs::write(
            run_dir.join("trace.json"),
            r#"{"steps":[],"result":"passed","reason":"执行员认为通过"}"#,
        )
        .unwrap();
        let assign = RunAssign {
            run_id,
            case: CaseSpec {
                code: "TC-节点".into(),
                title: "节点接入".into(),
                preconditions: vec![],
                steps: vec![],
                expected: vec![CaseExpected {
                    idx: 1,
                    text: "出现确认标记".into(),
                }],
                data: json!({}),
            },
            device_serial: "测试设备".into(),
            executor: RoleModel {
                source: ModelSource::CodexExec,
                model: Some("执行员模型".into()),
                effort: None,
            },
            judge: RoleModel {
                source: ModelSource::ClaudeCli,
                model: Some("判定员模型".into()),
                effort: Some("high".into()),
            },
            lease_id: None,
            needs_cleanup: false,
            limits: Limits {
                max_steps: 5,
                timeout_s: 10,
            },
        };
        Self {
            dir,
            run_dir,
            assign,
        }
    }

    fn config(&self, evidence_bin: PathBuf) -> RealConfig {
        RealConfig {
            platform_http: "http://127.0.0.1/".parse().unwrap(),
            node_token: String::new(),
            python: String::new(),
            automation_dir: PathBuf::new(),
            audio_dir: PathBuf::new(),
            state_dir: self.dir.clone(),
            runs_dir: self.dir.clone(),
            mcp_bin: PathBuf::new(),
            evidence_bin,
            package: String::new(),
            activity: String::new(),
            bottom_shift: HashMap::new(),
            screen: (720, 1600),
            app_info: String::new(),
        }
    }

    fn judge_opts(&self, script: &str) -> ClaudeOpts {
        let claude = self.dir.join("claude.sh");
        // 登录检查单独返回会员状态，实际判定才记录提示词和参数。
        fs::write(
            &claude,
            format!(
                r#"set -eu
if [ "$#" -eq 5 ] && [ "$1" = --setting-sources ] && [ -z "$2" ] &&
    [ "$3" = auth ] && [ "$4" = status ] && [ "$5" = --json ]; then
    printf '%s\n' '{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"pro"}}'
    exit 0
fi
cat > "$0.prompt"
printf '%s\n' "$@" > "$0.args"
{script}
"#
            ),
        )
        .unwrap();
        ClaudeOpts {
            bin: "/bin/sh".into(),
            pre_args: vec![claude.to_string_lossy().into()],
            stall_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
            ..Default::default()
        }
    }

    async fn messages(&self, script: &str, evidence_bin: PathBuf) -> Vec<NodeMsg> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runner = Runner::new(
            FinishedExecution {
                real: RealExecutor(self.config(evidence_bin)),
                judge_opts: self.judge_opts(script),
            },
            tx,
        );
        runner.spawn(Uuid::nil(), self.assign.clone());
        tokio::time::timeout(Duration::from_secs(15), async {
            while !runner.running().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("节点等待判定完成超时");
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn judge_done<'a>(&self, messages: &'a [NodeMsg]) -> &'a JudgeDone {
        let exec = messages.iter().position(|m| matches!(m, NodeMsg::ExecutorDone(d)
            if d.run_id == self.assign.run_id && d.result == Verdict::Passed && d.reason == "执行员认为通过"))
            .expect("执行员自评应上报");
        let Some((i, judge)) = messages.iter().enumerate().find_map(|(i, m)| match m {
            NodeMsg::JudgeDone(judge) => Some((i, judge)),
            _ => None,
        }) else {
            panic!("判定结果应上报")
        };
        assert!(i > exec);
        assert_eq!(judge.run_id, self.assign.run_id);
        judge
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn 执行员自评后用角色配置复核并上报独立判定() {
    let f = Fixture::new();
    let verdict = json!({
        "result": "failed", "items": [{
            "index": 1, "verdict": "failed", "screenshots": ["01_final.png"], "evidence": "画面没有确认标记"
        }],
    });
    let event = json!({"type": "result", "is_error": false, "result": verdict.to_string(), "total_cost_usd": 0.05});
    let msgs = f
        .messages(
            &format!(
                r#"
printf '%s\n' '["01_final.png"]' > served.json
cat <<'JUDGE_RESULT'
{event}
JUDGE_RESULT"#
            ),
            "mcp-evidence".into(),
        )
        .await;
    let judge = f.judge_done(&msgs);
    assert_eq!(
        (judge.judge_status, judge.final_result),
        (JudgeStatus::Completed, FinalResult::Failed)
    );
    assert_eq!(judge.items[0].file_refs, ["01_final.png"]);
    assert_eq!(judge.items[0].evidence, "画面没有确认标记");
    assert!(f.run_dir.join("served.json").is_file());
    let args = fs::read_to_string(f.dir.join("claude.sh.args")).unwrap();
    assert!(args.contains("--model\n判定员模型\n") && args.contains("--effort\nhigh\n"));
    assert!(!args.contains("执行员模型"));
    let prompt = fs::read_to_string(f.dir.join("claude.sh.prompt")).unwrap();
    assert!(prompt.contains("- reason: 执行员认为通过") && prompt.contains("仅供参考"));
    assert!(msgs.iter().any(|m| matches!(m, NodeMsg::Log(l) if l.msg.contains("用时") && l.msg.contains("0.050000 美元"))));
}

#[tokio::test]
async fn 只有步骤文字引用时节点照常上报判定() {
    let f = Fixture::new();
    fs::remove_dir_all(f.run_dir.join("shots")).unwrap();
    replace_trace(
        &f.run_dir,
        &[json!({"idx": 1, "action": "tap", "after_labels": ["操作失败"]})],
        Some("passed"),
    );
    let verdict = json!({
        "result": "failed", "items": [{
            "index": 1, "verdict": "failed", "screenshots": ["step:1"], "evidence": "提示操作失败"
        }],
    });
    let event = json!({"type": "result", "is_error": false, "result": verdict.to_string()});
    let msgs = f
        .messages(
            &format!(
                r#"
printf '%s\n' '["step:1"]' > served.json
cat <<'JUDGE_RESULT'
{event}
JUDGE_RESULT"#
            ),
            "mcp-evidence".into(),
        )
        .await;
    let judge = f.judge_done(&msgs);
    assert_eq!(
        (judge.judge_status, judge.final_result),
        (JudgeStatus::Completed, FinalResult::Failed)
    );
    assert_eq!(judge.items[0].file_refs, ["step:1"]);
    assert_eq!(judge.items[0].evidence, "提示操作失败");
}

#[tokio::test]
async fn 判定失败时节点上报待确认而非执行员自评() {
    let f = Fixture::new();
    let msgs = f
        .messages(
            r#"echo '{"type":"result","is_error":false,"result":"{损坏的结果}"}'"#,
            "mcp-evidence".into(),
        )
        .await;
    let judge = f.judge_done(&msgs);
    assert_eq!(
        (judge.judge_status, judge.final_result),
        (JudgeStatus::Failed, FinalResult::Uncertain)
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, NodeMsg::Log(l) if l.msg.contains("无法解析")))
    );
}

#[tokio::test]
async fn 未配置证据服务或来源不匹配时节点不运行判定() {
    let mut f = Fixture::new();
    let messages = f.messages("exit 91", PathBuf::new()).await;
    let j = f.judge_done(&messages);
    assert_eq!(
        (j.judge_status, j.final_result),
        (JudgeStatus::NotRun, FinalResult::Uncertain)
    );
    f.assign.judge.source = ModelSource::CodexExec;
    let messages = f.messages("exit 91", "mcp-evidence".into()).await;
    let j = f.judge_done(&messages);
    assert_eq!(
        (j.judge_status, j.final_result),
        (JudgeStatus::NotRun, FinalResult::Uncertain)
    );
    assert!(!f.dir.join("claude.sh.prompt").exists());
}

#[tokio::test]
async fn 判定额度耗尽时保留待确认并上报会员额度() {
    let f = Fixture::new();
    let msgs = f
        .messages(
            r#"echo 'reached your weekly limit' >&2
exit 1"#,
            "mcp-evidence".into(),
        )
        .await;
    let judge = f.judge_done(&msgs);
    assert_eq!(
        (judge.judge_status, judge.final_result),
        (JudgeStatus::Failed, FinalResult::Uncertain)
    );
    assert!(
        matches!(msgs.last(), Some(NodeMsg::QuotaExhausted(q)) if q.source == ModelSource::ClaudeCli && q.run_id.is_none())
    );
}

// ---------------------------------------------------------------- 执行员自标「受阻」的处理

const JUDGE_PASSES: &str = r#"
printf '%s\n' '["01_final.png"]' > served.json
cat <<'JUDGE_RESULT'
{"type":"result","is_error":false,"result":"{\"result\":\"passed\",\"items\":[{\"index\":1,\"verdict\":\"passed\",\"screenshots\":[\"01_final.png\"],\"evidence\":\"原图里有确认标记\"}]}","total_cost_usd":0.01}
JUDGE_RESULT"#;

fn set_trace(f: &Fixture, result: &str, steps: &str) {
    fs::write(f.run_dir.join("trace.json"), format!(r#"{{"steps":{steps},"result":"{result}","reason":"图标被编号遮住，确认不了"}}"#)).unwrap();
}

#[tokio::test]
async fn 执行员标受阻但留下了步骤时交给独立判定员() {
    // 回归：执行员走完步骤却因证据不足标了 blocked，之前被直接判「卡住」而跳过 Judge，
    // 结果错过了 Judge 看原图就能确认的通过
    let f = Fixture::new();
    set_trace(&f, "blocked", r#"[{"idx":1,"action":"tap"}]"#);
    let msgs = f.messages(JUDGE_PASSES, "mcp-evidence".into()).await;
    assert!(!msgs.iter().any(|m| matches!(m, NodeMsg::RunBlocked(_))), "有证据时不能判受阻：{msgs:?}");
    let done = msgs.iter().find_map(|m| if let NodeMsg::ExecutorDone(d) = m { Some(d) } else { None }).expect("要上报执行员结论");
    assert_eq!(done.result, Verdict::Uncertain, "执行员没把握，自评记为待确认");
    let judge = msgs.iter().find_map(|m| if let NodeMsg::JudgeDone(j) = m { Some(j) } else { None }).expect("要交给 Judge");
    assert_eq!((judge.judge_status, judge.final_result), (JudgeStatus::Completed, FinalResult::Passed));
}

#[tokio::test]
async fn 执行员标受阻且没有任何步骤时仍判受阻() {
    let f = Fixture::new();
    set_trace(&f, "blocked", "[]");
    let msgs = f.messages(JUDGE_PASSES, "mcp-evidence".into()).await;
    assert!(msgs.iter().any(|m| matches!(m, NodeMsg::RunBlocked(_))), "一步都没做就受阻是真受阻：{msgs:?}");
    assert!(!msgs.iter().any(|m| matches!(m, NodeMsg::JudgeDone(_))), "没有证据不调用 Judge");
}

const TEST_USER: &str = "synthetic-node-user@hafa.invalid";
const TEST_PASSWORD: &str = "synthetic-node-password!?";
const TEST_TOKEN: &str = "synthetic-node-auth";

struct FakeHttp {
    router: Router,
    server: Option<tokio::task::JoinHandle<()>>,
    base: reqwest::Url,
    calls: Arc<AtomicUsize>,
}

impl FakeHttp {
    async fn new(lease: Uuid, node: Uuid, status: StatusCode, body: String) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let received = calls.clone();
        let router = Router::new().route("/api/leases/{lease}/credentials", get(move |RoutePath(id): RoutePath<Uuid>, headers: HeaderMap| {
            let body = body.clone();
            let received = received.clone();
            async move {
                let authorized = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TEST_TOKEN}"));
                let caller = headers.get("X-Node-Id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
                if id != lease || !authorized || caller != Some(node) {
                    return (StatusCode::FORBIDDEN, [(header::CACHE_CONTROL, "no-store")], String::new());
                }
                received.fetch_add(1, Ordering::SeqCst);
                (status, [(header::CACHE_CONTROL, "no-store")], body)
            }
        }));
        Self::serve(router, calls).await
    }

    async fn serve(router: Router, calls: Arc<AtomicUsize>) -> Self {
        let (base, server) = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => {
                let base = format!("http://{}/", listener.local_addr().unwrap()).parse().unwrap();
                let app = router.clone();
                let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                (base, Some(server))
            }
            // 沙箱禁止绑定回环端口时，仍将完整 HTTP 请求交给同一个 Axum 路由处理。
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => ("http://127.0.0.1/".parse().unwrap(), None),
            Err(error) => panic!("本地假 HTTP 服务启动失败：{error}"),
        };
        Self { router, server, base, calls }
    }

    async fn send(&self, builder: reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
        if self.server.is_some() {
            return builder.send().await;
        }
        let built = builder.build()?;
        let body = built.body().and_then(reqwest::Body::as_bytes).map_or_else(Vec::new, <[u8]>::to_vec);
        let mut request = Request::builder().method(built.method()).uri(built.url().path()).body(Body::from(body)).unwrap();
        request.headers_mut().clone_from(built.headers());
        let response = call_http(self.router.clone().into_service::<Body>(), request).await;
        let (parts, body) = response.into_parts();
        let body = to_bytes(body, 8192).await.unwrap();
        Ok(axum::http::Response::from_parts(parts, body).into())
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

async fn call_http<S>(mut service: S, request: Request<Body>) -> axum::response::Response
where
    S: ServiceExt<Request<Body>, Response = axum::response::Response, Error = Infallible>,
{
    service.call(request).await.unwrap()
}

struct SetupOnly {
    real: RealExecutor,
    response: Option<FakeHttp>,
}

impl RunExecutor for SetupOnly {
    async fn execute(&self, assign: RunAssign, rep: Reporter) {
        let send = |request| async move { self.response.as_ref().expect("这次 setup 不需要取凭证").send(request).await };
        if self.real.setup(&assign, &rep, send).await.is_some() {
            rep.judge_done(JudgeStatus::NotRun, FinalResult::Uncertain, vec![]);
        }
    }
}

impl Fixture {
    async fn setup_messages(&self, node: Uuid, http: Option<FakeHttp>, script: &str) -> Vec<NodeMsg> {
        // /bin/sh 的 -m 选项之后可直接读取脚本，沿用真实 setup_runner 的命令和 stdin 通道。
        fs::write(self.dir.join("hafa_automation"), script).unwrap();
        let mut config = self.config(PathBuf::new());
        config.python = "/bin/sh".into();
        config.automation_dir = self.dir.clone();
        config.node_token = TEST_TOKEN.into();
        if let Some(http) = &http {
            config.platform_http = http.base.clone();
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runner = Runner::new(SetupOnly { real: RealExecutor(config), response: http }, tx);
        runner.spawn(node, self.assign.clone());
        tokio::time::timeout(Duration::from_secs(10), async {
            while !runner.running().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("假 setup 等待超时");
        let messages: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let serialized = serde_json::to_string(&messages).unwrap();
        assert!(!serialized.contains(TEST_USER) && !serialized.contains(TEST_PASSWORD) && !serialized.contains(TEST_TOKEN));
        messages
    }
}

#[tokio::test]
async fn 有租约时用节点身份取凭证并在恢复环境后经_stdin_交给_setup() {
    let mut f = Fixture::new();
    let (lease, node, account) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    f.assign.lease_id = Some(lease);
    f.assign.needs_cleanup = true;
    let body = json!({"account_id": account, "username": TEST_USER, "password": TEST_PASSWORD}).to_string();
    let http = FakeHttp::new(lease, node, StatusCode::OK, body).await;
    let calls = http.calls.clone();
    let event = json!({"type": "step", "msg": format!("{TEST_USER} {TEST_PASSWORD}")});
    let script = format!(r#"set -eu
if [ "$1" = '--teardown' ]; then
    test "$#" -eq 1
    test -z "$(cat)"
    printf 'teardown\n' >> "$HAFA_STATE_DIR/phases"
    echo '{{"type":"done","phase":"teardown"}}'
    exit 0
fi
test "$1" = '--case'
test "$3" = '--account-id'
test "$4" = '{account}'
test "$5" = '--credentials-stdin'
test "$#" -eq 5
input=$(cat)
case "$input" in *'"username":"{TEST_USER}"'*) ;; *) exit 91 ;; esac
case "$input" in *'"password":"{TEST_PASSWORD}"'*) ;; *) exit 92 ;; esac
test -z "${{NODE_TOKEN-}}"
printf 'setup\n' >> "$HAFA_STATE_DIR/phases"
printf '%s\n' '{event}'
echo '{{"type":"done","phase":"setup"}}'
"#);
    let msgs = f.setup_messages(node, Some(http), &script).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fs::read_to_string(f.dir.join("phases")).unwrap(), "teardown\nsetup\n");
    assert!(msgs.iter().any(|m| matches!(m, NodeMsg::Log(log) if log.msg.contains("{{account.password}}"))));
    assert!(matches!(msgs.last(), Some(NodeMsg::JudgeDone(_))));
}

#[tokio::test]
async fn 凭证接口失败或响应损坏时受阻且不转发响应内容() {
    for (status, hint) in [(StatusCode::GONE, "HTTP 410"), (StatusCode::OK, "响应格式错误")] {
        let mut f = Fixture::new();
        let (lease, node) = (Uuid::new_v4(), Uuid::new_v4());
        f.assign.lease_id = Some(lease);
        f.assign.needs_cleanup = true;
        let http = FakeHttp::new(lease, node, status, format!("{TEST_USER} {TEST_PASSWORD}")).await;
        let calls = http.calls.clone();
        let msgs = f.setup_messages(node, Some(http), "touch \"$HAFA_STATE_DIR/phases\"").await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!f.dir.join("phases").exists());
        assert!(matches!(msgs.as_slice(), [NodeMsg::RunBlocked(b)] if b.reason == BlockedReason::Setup && b.detail.contains(hint)));
    }
}

#[tokio::test]
async fn 有清理标志但恢复失败时不启动_setup() {
    let mut f = Fixture::new();
    let (lease, node) = (Uuid::new_v4(), Uuid::new_v4());
    f.assign.lease_id = Some(lease);
    f.assign.needs_cleanup = true;
    let body = json!({"account_id": Uuid::new_v4(), "username": TEST_USER, "password": TEST_PASSWORD}).to_string();
    let http = FakeHttp::new(lease, node, StatusCode::OK, body).await;
    let msgs = f.setup_messages(node, Some(http), r#"printf '%s\n' "$1" >> "$HAFA_STATE_DIR/phases"
exit 1"#).await;
    assert_eq!(fs::read_to_string(f.dir.join("phases")).unwrap(), "--teardown\n");
    assert!(matches!(msgs.as_slice(), [NodeMsg::RunBlocked(b)] if b.reason == BlockedReason::Setup && b.detail.contains("恢复环境失败")));
}

#[tokio::test]
async fn 无租约时跳过登录且不请求凭证() {
    let f = Fixture::new();
    let msgs = f.setup_messages(Uuid::new_v4(), None, r#"set -eu
test "$1" = '--case'
test "$3" = '--skip-login'
test "$#" -eq 3
test -z "$(cat)"
echo '{"type":"done","phase":"setup"}'
"#).await;
    assert!(matches!(msgs.last(), Some(NodeMsg::JudgeDone(_))));
}

type Uploads = Arc<Mutex<Vec<(String, Bytes)>>>;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

async fn fake_upload_server(run: Uuid, node: Uuid, reject: Option<&'static str>) -> (FakeHttp, Uploads) {
    let seen = Uploads::default();
    let uploads = seen.clone();
    let router = Router::new().route("/api/runs/{id}/files/{name}", put(move |
        RoutePath((id, name)): RoutePath<(Uuid, String)>, headers: HeaderMap, bytes: Bytes,
    | {
        let uploads = uploads.clone();
        async move {
            let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
            let request_node = headers.get("X-Node-Id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
            if id != run || auth != Some(&format!("Bearer {TEST_TOKEN}")) || request_node != Some(node) {
                return StatusCode::FORBIDDEN;
            }
            let status = if reject == Some(name.as_str()) { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::NO_CONTENT };
            uploads.lock().unwrap().push((name, bytes));
            status
        }
    }));
    (FakeHttp::serve(router, Arc::default()).await, seen)
}

struct EvidenceExecution {
    real: RealExecutor,
    http: FakeHttp,
    advance: Option<Arc<Semaphore>>,
    judge_opts: ClaudeOpts,
}

fn replace_trace(dir: &Path, steps: &[serde_json::Value], result: Option<&str>) {
    let temp = dir.join("trace.tmp");
    let trace = json!({"steps": steps, "result": result, "reason": "执行员认为通过"});
    fs::write(&temp, trace.to_string()).unwrap();
    fs::rename(temp, dir.join("trace.json")).unwrap();
}

impl RunExecutor for EvidenceExecution {
    async fn execute(&self, assign: RunAssign, rep: Reporter) {
        let dir = self.real.0.runs_dir.join(assign.run_id.to_string());
        rep.started();
        let fake_model = async {
            if let Some(advance) = &self.advance {
                let mut steps = Vec::new();
                for idx in [1, 2] {
                    let shot = format!("shots/{idx:02}_after.png");
                    fs::write(dir.join(&shot), PNG_SIGNATURE).unwrap();
                    fs::write(dir.join(format!("shots/{idx:02}_observe.png")), PNG_SIGNATURE).unwrap();
                    steps.push(json!({"idx": idx, "at_ms": 1_700_000_000_000_i64 + i64::from(idx), "action": "tap", "target": "选择框", "shot": shot, "new_labels": ["已选中"]}));
                    replace_trace(&dir, &steps, None);
                    // 测试接到实时步骤和对应截图后才让假执行器继续。
                    advance.acquire().await.unwrap().forget();
                }
                replace_trace(&dir, &steps, Some("passed"));
                fs::write(dir.join("shots/03_final.png"), PNG_SIGNATURE).unwrap();
            }
            fs::write(dir.join("final_page.txt"), "页面已选中").unwrap();
            Summary { model: Some("测试执行模型".into()), effort: Some("max".into()), ..Summary::default() }
        };
        let execution = track_execution(&dir, &rep, &self.http.base, &self.real.0.node_token, fake_model,
            |request| self.http.send(request)).await;
        self.real.report_execution(&assign, rep, &dir, execution, self.judge_opts.clone()).await;
    }
}

async fn received_live_step(rx: &mut mpsc::UnboundedReceiver<NodeMsg>, expected_idx: u32) {
    tokio::time::timeout(Duration::from_secs(6), async {
        while let Some(message) = rx.recv().await {
            match message {
                NodeMsg::Step(step) => {
                    assert_eq!(step.idx, expected_idx, "只实时上报新增的步骤");
                    assert_eq!(step.at_ms, 1_700_000_000_000_i64 + i64::from(expected_idx));
                    assert_eq!(step.args["target"], "选择框");
                    assert_eq!(step.result["new_labels"], json!(["已选中"]));
                    assert_eq!(step.file_ref, Some(format!("shots/{expected_idx:02}_after.png")));
                    return;
                }
                NodeMsg::ExecutorDone(_) | NodeMsg::JudgeDone(_) => panic!("步骤应该先于执行结束上报"),
                _ => {}
            }
        }
        panic!("没有收到实时步骤");
    }).await.expect("等待实时步骤超时");
}

async fn received_shots(uploads: &Uploads, names: &[&str]) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if names.iter().all(|name| uploads.lock().unwrap().iter().any(|(n, b)| n == name && b.as_ref() == PNG_SIGNATURE)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }).await.expect("没有在执行期间上传截图");
}

async fn run_messages(rx: &mut mpsc::UnboundedReceiver<NodeMsg>) -> Vec<NodeMsg> {
    tokio::time::timeout(Duration::from_secs(12), async {
        let mut messages = Vec::new();
        while let Some(message) = rx.recv().await {
            let done = matches!(message, NodeMsg::JudgeDone(_));
            messages.push(message);
            if done {
                return messages;
            }
        }
        panic!("假执行没有给出判定");
    }).await.expect("等待假执行结束超时")
}

#[tokio::test]
async fn 执行期间轮询上报新增步骤和两类截图_结束补传() {
    let f = Fixture::new();
    let node = Uuid::new_v4();
    let (http, uploads) = fake_upload_server(f.assign.run_id, node, None).await;
    let advance = Arc::new(Semaphore::new(0));
    let mut config = f.config("mcp-evidence".into());
    config.node_token = TEST_TOKEN.into();
    let judge = JUDGE_PASSES.replace("01_final.png", "02_after.png");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = Runner::new(EvidenceExecution {
        real: RealExecutor(config), http, advance: Some(advance.clone()), judge_opts: f.judge_opts(&judge),
    }, tx);
    runner.spawn(node, f.assign.clone());

    received_live_step(&mut rx, 1).await;
    received_shots(&uploads, &["01_observe.png", "01_after.png"]).await;
    assert!(!uploads.lock().unwrap().iter().any(|(n, _)| n == "final_page.txt"));
    advance.add_permits(1);
    received_live_step(&mut rx, 2).await;
    received_shots(&uploads, &["02_observe.png", "02_after.png"]).await;
    advance.add_permits(1);

    let messages = run_messages(&mut rx).await;
    assert_eq!(messages.iter().filter_map(|m| match m { NodeMsg::Step(s) => Some(s.idx), _ => None }).collect::<Vec<_>>(), [1, 2]);
    let judge = f.judge_done(&messages);
    assert_eq!(judge.judge_status, JudgeStatus::Completed);
    assert_eq!(judge.items[0].file_refs, ["02_after.png"]);
    let uploaded = uploads.lock().unwrap();
    assert!(uploaded.iter().any(|(name, bytes)| name == "final_page.txt" && bytes.as_ref() == "页面已选中".as_bytes()));
    assert!(uploaded.iter().any(|(name, bytes)| name == "03_final.png" && bytes.as_ref() == PNG_SIGNATURE));
    assert_eq!(uploaded.iter().filter(|(name, _)| name == "01_after.png").count(), 1);
}

#[tokio::test]
async fn 证据上传失败重试三次并跳过_执行和_judge_照常完成() {
    let f = Fixture::new();
    let node = Uuid::new_v4();
    let (http, uploads) = fake_upload_server(f.assign.run_id, node, Some("01_final.png")).await;
    let mut config = f.config("mcp-evidence".into());
    config.node_token = TEST_TOKEN.into();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = Runner::new(EvidenceExecution {
        real: RealExecutor(config), http, advance: None, judge_opts: f.judge_opts(JUDGE_PASSES),
    }, tx);
    runner.spawn(node, f.assign.clone());
    let messages = run_messages(&mut rx).await;
    assert_eq!(uploads.lock().unwrap().iter().filter(|(name, _)| name == "01_final.png").count(), 4);
    assert!(messages.iter().any(|m| matches!(m, NodeMsg::Log(log) if log.msg.contains("已重试 3 次并跳过"))));
    let judge = f.judge_done(&messages);
    assert_eq!((judge.judge_status, judge.final_result), (JudgeStatus::Completed, FinalResult::Passed));
    assert!(judge.items[0].file_refs.is_empty());
    assert!(judge.items[0].evidence.contains("截图上传失败"));
}

#[derive(Clone, Copy)]
enum ModelEnd {
    Success,
    Rollover,
    Timeout,
    Panic,
    Cancel,
}

#[derive(Default)]
struct RecordingUploads {
    files: Vec<String>,
    indexes: Vec<serde_json::Value>,
}

struct RecordedExecution {
    real: RealExecutor,
    adb: FakeAdb,
    credentials: Arc<FakeHttp>,
    uploads: Arc<Mutex<RecordingUploads>>,
    node: Uuid,
    mode: ModelEnd,
    fail_first_upload: bool,
    judge_opts: ClaudeOpts,
}

impl RunExecutor for RecordedExecution {
    async fn execute(&self, assign: RunAssign, rep: Reporter) {
        let serial = assign.device_serial.clone();
        let executor_serial = serial.clone();
        let run_id = assign.run_id;
        let phases = self.real.0.state_dir.join("phases");
        let (credentials, adb, uploads) = (self.credentials.clone(), self.adb.clone(), self.uploads.clone());
        let (node, fail_first_upload) = (self.node, self.fail_first_upload);
        self.real.execute_with(assign, rep, self.adb.clone(), |_, _, _| async {
            use std::io::Write;
            let mut log = fs::OpenOptions::new().append(true).open(&phases).unwrap();
            writeln!(log, "executor").unwrap();
            match self.mode {
                ModelEnd::Rollover => {
                    self.adb.finish_segment(&executor_serial);
                    self.adb.wait_for_segment(&executor_serial, 2).await;
                }
                ModelEnd::Panic => panic!("模拟执行员崩溃"),
                ModelEnd::Cancel => std::future::pending::<()>().await,
                ModelEnd::Success | ModelEnd::Timeout => {}
            }
            Summary { timed_out: matches!(self.mode, ModelEnd::Timeout), ..Summary::default() }
        }, move |builder| {
            let (credentials, adb, serial, uploads) = (credentials.clone(), adb.clone(), serial.clone(), uploads.clone());
            async move {
                if builder.try_clone().and_then(|copy| copy.build().ok())
                    .is_some_and(|request| request.method() == reqwest::Method::GET) {
                    return credentials.send(builder).await;
                }
                let request = builder.build()?;
                let mut status = StatusCode::NO_CONTENT;
                let name = request.url().path_segments().unwrap().next_back().unwrap();
                if name.ends_with(".mp4") || name == "recording-index" {
                    assert_eq!(request.method(), reqwest::Method::PUT);
                    assert_eq!(request.headers()[header::AUTHORIZATION], format!("Bearer {TEST_TOKEN}"));
                    assert_eq!(request.headers()["X-Node-Id"], node.to_string());
                    assert!(request.url().path().starts_with(&format!("/api/runs/{run_id}/")));
                    assert!(adb.active(&serial).is_none());
                    let mut uploads = uploads.lock().unwrap();
                    if name.ends_with(".mp4") {
                        assert_eq!(request.headers()[header::CONTENT_TYPE], "video/mp4");
                        assert_eq!(request.headers()[header::CONTENT_LENGTH], FAKE_MP4.len().to_string());
                        assert!(request.body().unwrap().as_bytes().is_none(), "录屏必须流式上传");
                        uploads.files.push(name.into());
                        if fail_first_upload && name == "rec_01.mp4" {
                            status = StatusCode::SERVICE_UNAVAILABLE;
                        }
                    } else {
                        let body = request.body().and_then(reqwest::Body::as_bytes).unwrap();
                        uploads.indexes.push(serde_json::from_slice(body).unwrap());
                    }
                }
                Ok(axum::http::Response::builder().status(status).body("").unwrap().into())
            }
        }, self.judge_opts.clone()).await;
    }
}

impl Fixture {
    async fn recording_executor(&mut self, mode: ModelEnd) -> RecordedExecution {
        let (node, lease) = (Uuid::new_v4(), Uuid::new_v4());
        self.assign.lease_id = Some(lease);
        self.assign.device_serial = format!("测试设备-{}", self.assign.run_id);
        let credentials = FakeHttp::new(lease, node, StatusCode::OK, json!({
            "account_id": Uuid::new_v4(), "username": TEST_USER, "password": TEST_PASSWORD
        }).to_string()).await;
        let event = json!({"type": "step", "msg": format!("登录 {TEST_USER} {TEST_PASSWORD}")});
        let script = format!(r#"set -eu
if [ "$1" = '--teardown' ]; then
    test -z "$(cat)"
    printf 'teardown\n' >> "$HAFA_STATE_DIR/phases"
    echo '{{"type":"done","phase":"teardown"}}'
    exit 0
fi
input=$(cat)
case "$input" in *'"username":"{TEST_USER}"'*) ;; *) exit 91 ;; esac
case "$input" in *'"password":"{TEST_PASSWORD}"'*) ;; *) exit 92 ;; esac
printf '%s\n' '{event}'
printf 'setup\n' >> "$HAFA_STATE_DIR/phases"
echo '{{"type":"done","phase":"setup"}}'
"#);
        fs::write(self.dir.join("hafa_automation"), script).unwrap();
        let mut config = self.config("mcp-evidence".into());
        config.python = "/bin/sh".into();
        config.automation_dir = self.dir.clone();
        config.platform_http = credentials.base.clone();
        config.node_token = TEST_TOKEN.into();
        let mut adb = FakeAdb::default();
        adb.phases = Some(self.dir.join("phases"));
        RecordedExecution {
            real: RealExecutor(config), adb, credentials: Arc::new(credentials), uploads: Arc::new(Mutex::new(RecordingUploads::default())),
            node, mode, fail_first_upload: false, judge_opts: self.judge_opts(JUDGE_PASSES),
        }
    }
}

fn 不含凭证(text: &str) {
    for secret in [TEST_USER, TEST_PASSWORD, TEST_TOKEN] {
        assert!(!text.contains(secret), "录屏或日志泄露了凭证");
    }
}

#[tokio::test]
async fn 录屏只在登录后到teardown前运行且索引日志不含凭证() {
    let mut fixture = Fixture::new();
    let execution = fixture.recording_executor(ModelEnd::Success).await;
    let (node, adb, uploads) = (execution.node, execution.adb.clone(), execution.uploads.clone());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = Runner::new(execution, tx);
    runner.spawn(node, fixture.assign.clone());

    let messages = run_messages(&mut rx).await;
    assert_eq!(fs::read_to_string(fixture.dir.join("phases")).unwrap(), "setup\nstart\nexecutor\nstop\nteardown\n");
    assert!(adb.active(&fixture.assign.device_serial).is_none());
    assert!(messages.iter().any(|msg| matches!(msg, NodeMsg::Log(log) if log.msg.contains("{{account.password}}"))));
    不含凭证(&serde_json::to_string(&messages).unwrap());
    let index = fs::read_to_string(fixture.run_dir.join("recording/index.json")).unwrap();
    不含凭证(&index);
    let uploads = uploads.lock().unwrap();
    assert_eq!(uploads.files, ["rec_01.mp4"]);
    assert_eq!(uploads.indexes, [serde_json::from_str::<serde_json::Value>(&index).unwrap()]);
}

#[tokio::test]
async fn 录屏启动失败只告警不改变执行和判定结果() {
    let mut fixture = Fixture::new();
    let mut execution = fixture.recording_executor(ModelEnd::Success).await;
    execution.adb.start_error = Some(TEST_PASSWORD);
    let node = execution.node;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = Runner::new(execution, tx);
    runner.spawn(node, fixture.assign.clone());
    let messages = run_messages(&mut rx).await;
    assert!(messages.iter().any(|msg| matches!(msg, NodeMsg::Log(log) if log.level == "warn" && log.msg.contains("启动录屏失败"))));
    assert!(messages.iter().any(|msg| matches!(msg, NodeMsg::ExecutorDone(done) if done.result == Verdict::Passed)));
    assert_eq!(fixture.judge_done(&messages).judge_status, JudgeStatus::Completed);
    assert_eq!(fixture.judge_done(&messages).final_result, FinalResult::Passed);
    不含凭证(&serde_json::to_string(&messages).unwrap());
    不含凭证(&fs::read_to_string(fixture.run_dir.join("recording/index.json")).unwrap());
}

#[tokio::test]
async fn 录屏上传失败重试三次且索引只提交成功分段() {
    let mut fixture = Fixture::new();
    let mut execution = fixture.recording_executor(ModelEnd::Rollover).await;
    execution.fail_first_upload = true;
    let (node, uploads) = (execution.node, execution.uploads.clone());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = Runner::new(execution, tx);
    runner.spawn(node, fixture.assign.clone());
    let messages = run_messages(&mut rx).await;
    let uploads = uploads.lock().unwrap();
    assert_eq!(uploads.files, ["rec_01.mp4", "rec_01.mp4", "rec_01.mp4", "rec_01.mp4", "rec_02.mp4"]);
    assert_eq!(uploads.indexes.len(), 1);
    assert_eq!(uploads.indexes[0]["segments"].as_array().unwrap().len(), 1);
    assert_eq!(uploads.indexes[0]["segments"][0]["seq"], 2);
    assert!(messages.iter().any(|msg| matches!(msg, NodeMsg::Log(log) if log.level == "warn" && log.msg.contains("已重试 3 次并跳过"))));
    assert!(messages.iter().any(|msg| matches!(msg, NodeMsg::ExecutorDone(done) if done.result == Verdict::Passed)));
    assert_eq!(fixture.judge_done(&messages).final_result, FinalResult::Passed);
}

#[tokio::test]
async fn 执行员超时和panic都不会遗留录屏进程() {
    for mode in [ModelEnd::Timeout, ModelEnd::Panic] {
        let mut fixture = Fixture::new();
        let execution = fixture.recording_executor(mode).await;
        let (node, adb, uploads) = (execution.node, execution.adb.clone(), execution.uploads.clone());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runner = Runner::new(execution, tx);
        runner.spawn(node, fixture.assign.clone());
        tokio::time::timeout(Duration::from_secs(10), async {
            while !runner.running().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap();
        let _device = tokio::time::timeout(Duration::from_secs(3), lock_device(&fixture.assign.device_serial)).await.unwrap();
        assert!(adb.active(&fixture.assign.device_serial).is_none());
        let phases = fs::read_to_string(fixture.dir.join("phases")).unwrap();
        if matches!(mode, ModelEnd::Panic) {
            assert_eq!(phases, "setup\nstart\nexecutor\nstop\n");
            assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|msg| matches!(msg, NodeMsg::RunBlocked(_))));
        } else {
            assert_eq!(phases, "setup\nstart\nexecutor\nstop\nteardown\n");
        }
        等待末段上传(&uploads).await;
    }
}

async fn 等待末段上传(uploads: &Mutex<RecordingUploads>) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !uploads.lock().unwrap().indexes.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    let uploads = uploads.lock().unwrap();
    assert_eq!(uploads.files, ["rec_01.mp4"]);
    assert_eq!(uploads.indexes[0]["segments"][0]["file"], "rec_01.mp4");
}

#[tokio::test]
async fn 取消run会停止录屏并上传末段而不启动判定() {
    let mut fixture = Fixture::new();
    let execution = fixture.recording_executor(ModelEnd::Cancel).await;
    let (node, adb, uploads) = (execution.node, execution.adb.clone(), execution.uploads.clone());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = Runner::new(execution, tx);
    runner.spawn(node, fixture.assign.clone());
    adb.wait_for_segment(&fixture.assign.device_serial, 1).await;
    assert!(runner.abort(fixture.assign.run_id));
    等待末段上传(&uploads).await;
    assert!(adb.active(&fixture.assign.device_serial).is_none());
    assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(|msg| matches!(msg, NodeMsg::JudgeDone(_))));
}
