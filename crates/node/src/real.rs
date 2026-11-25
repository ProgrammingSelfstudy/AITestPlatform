//! 真实执行器：setup → GPT 执行员（经设备 MCP 操作真机，实时上报轨迹）→ teardown → Claude 独立判定。
//!
//! - 配置只读证据服务后，Claude 依据本次轨迹复核预期。未启用判定时最终结果待确认；
//! - 有账号租约时，在 setup 前一次性领取凭证。无租约时跳过登录；
//! - 只支持 `codex_exec` 作为执行员来源，其它来源如实报受阻。

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent::claude_cli::ClaudeOpts;
use agent::codex_exec::command::DeviceMcp;
use agent::codex_exec::{run_executor, CodexOpts};
use agent::judge::{run_judge, run_judge_agy};
use agent::prompts::{build_task_prompt, device_hints, generic_device_hints, AppKnowledge, DEVICE_TOOLS, SYSTEM_PROMPT};
use device::api::DeviceApi;
use protocol::{BlockedReason, FinalResult, JudgeOpinion, JudgeStatus, LeaseCredentials, ModelSource, RoleModel, RunAssign, Verdict};
use serde_json::json;
use uuid::Uuid;

use crate::recorder::{Adb, Recorder, SystemAdb, lock_device};
use crate::runner::{Reporter, RunExecutor};
use crate::setup_runner::{run_setup, AccountArg, SetupResult, SetupSpec};
use crate::uploader::{Execution, track_execution, upload_recordings};

/// 前置/收尾准备方式：
/// - `HafaScript`：调 Python 自动化脚本（`automation/hafa_automation`），用于有定制前置流程的 Hafa 用例；
/// - `Generic`：纯 Rust 通用 App 准备（唤醒解锁 → 启动被测应用；收尾停应用），不依赖 Python 和测试音频。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SetupMode {
    #[default]
    HafaScript,
    Generic,
}

#[derive(Clone)]
pub struct RealConfig {
    pub platform_http: reqwest::Url,
    pub node_token: String,
    pub setup_mode: SetupMode,
    /// 是否把 Hafa 语聊房专属坐标提示注入任务提示词；非 Hafa 应用或显式关闭时只传通用屏幕信息
    pub hafa_hints: bool,
    pub python: String,
    pub automation_dir: PathBuf,
    pub audio_dir: PathBuf,
    pub state_dir: PathBuf,
    /// 每个 run 的产出目录（轨迹、截图、日志）
    pub runs_dir: PathBuf,
    pub mcp_bin: PathBuf,
    /// 证据 MCP 可执行文件。空路径表示未启用独立判定。
    pub evidence_bin: PathBuf,
    pub package: String,
    pub activity: String,
    /// 设备序列号 → 底部坐标偏移（带导航栏的机型）
    pub bottom_shift: HashMap<String, i32>,
    pub screen: (i32, i32),
    /// 被测应用的已知信息（界面语言、文案对照等），原文交给执行员
    pub app_info: String,
}

pub struct RealExecutor(pub RealConfig);

impl RealExecutor {
    async fn teardown(&self, serial: &str) -> SetupResult {
        if self.0.setup_mode == SetupMode::Generic {
            return self.teardown_generic(&device::Android::new(serial)).await;
        }
        let mut t = self.spec(serial);
        t.teardown = true;
        run_setup(&t, |_| {}).await
    }

    pub(crate) async fn teardown_generic(&self, dev: &dyn DeviceApi) -> SetupResult {
        if self.0.package.is_empty() {
            return SetupResult::Ok;
        }
        match dev.stop_app(&self.0.package).await {
            Ok(()) => SetupResult::Ok,
            Err(e) => SetupResult::StepFailed { msg: format!("停止被测应用失败：{e}"), page: vec![] },
        }
    }

    /// 通用 App 前置准备（纯 Rust）：唤醒解锁 →（按需强停）→ 启动被测应用。
    /// 若用例绑定了账号租约，同样一次性拉取凭证并作为脱敏/注入清单传给设备 MCP（由 `input_text` 注入）。
    pub(crate) async fn setup_generic<F>(
        &self, a: &RunAssign, rep: &Reporter, dev: &dyn DeviceApi, send: impl FnOnce(reqwest::RequestBuilder) -> F + Send,
    ) -> Option<(i32, Vec<(String, String)>)>
    where
        F: Future<Output = reqwest::Result<reqwest::Response>> + Send,
    {
        let mut secrets = Vec::new();
        if let Some(lease_id) = a.lease_id {
            match self.fetch_account(lease_id, rep.node_id(), send).await {
                Ok(acc) => {
                    secrets.push((acc.username, "{{account.username}}".into()));
                    secrets.push((acc.password, "{{account.password}}".into()));
                }
                Err(detail) => {
                    rep.blocked(BlockedReason::Setup, detail);
                    return None;
                }
            }
        }
        rep.log("info", format!("通用前置准备开始：{}", a.case.code));
        match dev.ensure_awake().await {
            Ok(true) => rep.log("info", "[setup] 屏幕已唤醒并解锁".into()),
            Ok(false) => {
                rep.blocked(BlockedReason::Setup, "屏幕仍然锁定（可能设了 PIN / 图案），需要人工解锁".into());
                return None;
            }
            Err(e) => {
                rep.blocked(BlockedReason::Setup, format!("唤醒解锁设备失败：{e}"));
                return None;
            }
        }
        if a.needs_cleanup && !self.0.package.is_empty() {
            if let Err(e) = dev.stop_app(&self.0.package).await {
                rep.blocked(BlockedReason::Setup, format!("执行前停止被测应用失败：{e}"));
                return None;
            }
        }
        if !self.0.package.is_empty() && !self.0.activity.is_empty() {
            if let Err(e) = dev.launch_app(&self.0.package, &self.0.activity).await {
                rep.blocked(BlockedReason::Setup, format!("启动被测应用失败：{e}"));
                return None;
            }
            rep.log("info", format!("[setup] 已启动被测应用 {}/{}", self.0.package, self.0.activity));
        }
        let bottom_shift = self.0.bottom_shift.get(&a.device_serial).copied().unwrap_or(0);
        Some((bottom_shift, secrets))
    }

    fn spec(&self, serial: &str) -> SetupSpec {
        let c = &self.0;
        let mut s = SetupSpec::new(&c.python, &c.automation_dir, serial, &c.package, &c.activity);
        s.bottom_shift = c.bottom_shift.get(serial).copied().unwrap_or(0);
        s.audio_dir = c.audio_dir.clone();
        s.state_dir = c.state_dir.clone();
        s
    }

    async fn fetch_account<F>(
        &self, lease_id: Uuid, node_id: Uuid, send: impl FnOnce(reqwest::RequestBuilder) -> F + Send,
    ) -> Result<AccountArg, String>
    where
        F: Future<Output = reqwest::Result<reqwest::Response>> + Send,
    {
        let config = &self.0;
        let url = config.platform_http.join(&format!("/api/leases/{lease_id}/credentials"))
            .map_err(|_| "平台凭证接口地址无效")?;
        let client = crate::platform_client(Duration::from_secs(10))
            .map_err(|_| "无法初始化账号凭证请求")?;
        let request = crate::with_node_secret(client.get(url).bearer_auth(&config.node_token).header("X-Node-Id", node_id.to_string()));
        let response = send(request).await.map_err(|_| "请求账号凭证失败")?;
        if !response.status().is_success() {
            return Err(format!("领取账号凭证失败（HTTP {}）", response.status().as_u16()));
        }
        // JSON 解析错误也可能带响应内容，只上报固定文案。
        let LeaseCredentials { account_id, username, password } = response.json().await.map_err(|_| "账号凭证响应格式错误")?;
        Ok(AccountArg { id: account_id.to_string(), username, password })
    }
    async fn setup<F>(
        &self, a: &RunAssign, rep: &Reporter, send: impl FnOnce(reqwest::RequestBuilder) -> F + Send,
    ) -> Option<(i32, Vec<(String, String)>)>
    where
        F: Future<Output = reqwest::Result<reqwest::Response>> + Send,
    {
        if self.0.setup_mode == SetupMode::Generic {
            return self.setup_generic(a, rep, &device::Android::new(&a.device_serial), send).await;
        }
        let serial = &a.device_serial;
        let mut s = self.spec(serial);
        s.case = Some(a.case.code.clone());
        if let Some(lease_id) = a.lease_id {
            match self.fetch_account(lease_id, rep.node_id(), send).await {
                Ok(account) => s.account = Some(account),
                Err(detail) => {
                    rep.blocked(BlockedReason::Setup, detail);
                    return None;
                }
            }
        }
        if a.needs_cleanup && self.teardown(serial).await != SetupResult::Ok {
            rep.blocked(BlockedReason::Setup, "执行前恢复环境失败".into());
            return None;
        }
        rep.log("info", format!("setup 开始：{}", a.case.code));
        // setup_runner 会脱敏至少三个字符的值。更短的凭证只上报固定消息。
        let show_details = s.account.as_ref().is_none_or(|a| a.username.chars().count() >= 3 && a.password.chars().count() >= 3);
        let r = run_setup(&s, |e| {
            if show_details {
                rep.log("info", format!("[setup] {}", e.msg));
            }
        }).await;
        // 留下需要脱敏的值，交给设备 MCP 写轨迹和回显时替换成占位符
        let secrets: Vec<(String, String)> = s.account.take().map_or_else(Vec::new, |acc| {
            vec![(acc.username, "{{account.username}}".into()), (acc.password, "{{account.password}}".into())]
        });
        if r != SetupResult::Ok {
            self.teardown(serial).await;
            let detail = if show_details { format!("setup 没有成功：{r:?}") } else { "setup 没有成功".into() };
            rep.blocked(BlockedReason::Setup, detail);
            return None;
        }
        Some((s.bottom_shift, secrets))
    }
}

impl RunExecutor for RealExecutor {
    async fn execute(&self, a: RunAssign, rep: Reporter) {
        self.execute_with(a, rep, SystemAdb, |opts, prompt, dir| async move {
            run_executor(&opts, SYSTEM_PROMPT, &prompt, &dir).await
        }, reqwest::RequestBuilder::send, ClaudeOpts::default()).await;
    }
}

/// 把脱敏清单写到 `runs_dir/.secrets/<run_id>.json`（0600，不在 run 目录里，证据接口读不到）。
/// 没有要脱敏的值时返回 `None`。
fn write_secrets_file(runs_dir: &Path, run_id: uuid::Uuid, secrets: &[(String, String)]) -> std::io::Result<Option<PathBuf>> {
    if secrets.is_empty() {
        return Ok(None);
    }
    let dir = runs_dir.join(".secrets");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{run_id}.json"));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
        f.write_all(&serde_json::to_vec(secrets).expect("可序列化"))?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, serde_json::to_vec(secrets).expect("可序列化"))?;
    Ok(Some(path))
}

impl RealExecutor {
    async fn execute_with<F, R>(
        &self, a: RunAssign, rep: Reporter, adb: impl Adb,
        execute: impl FnOnce(CodexOpts, String, PathBuf) -> F + Send,
        send: impl Fn(reqwest::RequestBuilder) -> R + Send + Sync + 'static,
        judge_opts: ClaudeOpts,
    )
    where
        F: Future<Output = agent::codex_exec::events::Summary> + Send,
        R: Future<Output = reqwest::Result<reqwest::Response>> + Send + 'static,
    {
        let c = &self.0;
        if a.executor.source != ModelSource::CodexExec {
            return rep.blocked(BlockedReason::Other, format!("执行员来源 {:?} 还没有实现（目前只支持 codex_exec）", a.executor.source));
        }
        rep.started();
        let serial = a.device_serial.clone();
        let run_dir = c.runs_dir.join(a.run_id.to_string());
        if let Err(e) = std::fs::create_dir_all(&run_dir) {
            return rep.blocked(BlockedReason::Other, format!("创建执行目录失败：{e}"));
        }

        // 1. 前置状态
        let device = lock_device(&serial).await;
        let send = Arc::new(send);
        let Some((bottom_shift, secrets)) = self.setup(&a, &rep, &*send).await else { return };

        // 2. 执行员
        // 明文不放进 MCP 的环境变量（会经命令行参数出现在进程列表里），写 0600 文件，MCP 读完即删
        let secrets_file = match write_secrets_file(&c.runs_dir, a.run_id, &secrets) {
            Ok(f) => f,
            Err(e) => {
                self.teardown(&serial).await;
                return rep.blocked(BlockedReason::Other, format!("写脱敏清单失败：{e}"));
            }
        };
        let mut mcp_env = vec![];
        if let Some(f) = &secrets_file {
            mcp_env.push((agent::codex_exec::command::SECRETS_ENV.to_string(), f.to_string_lossy().into_owned()));
        }
        let mcp = DeviceMcp {
            command: c.mcp_bin.clone(),
            env: mcp_env.into_iter().chain(vec![
                ("HAFA_SERIAL".into(), serial.clone()),
                ("HAFA_RUN_DIR".into(), run_dir.to_string_lossy().into_owned()),
                ("HAFA_CASE_ID".into(), a.case.code.clone()),
                ("HAFA_MAX_STEPS".into(), a.limits.max_steps.to_string()),
                ("HAFA_MODEL".into(), a.executor.model.clone().unwrap_or_default()),
                ("HAFA_APP_PACKAGE".into(), c.package.clone()),
                ("HAFA_APP_ACTIVITY".into(), c.activity.clone()),
                ("HAFA_BOTTOM_SHIFT".into(), bottom_shift.to_string()),
            ]).chain(crate::params::mcp_env()).collect(),
        };
        let mut o = CodexOpts::new(mcp, DEVICE_TOOLS.iter().map(|t| t.to_string()).collect());
        o.model = a.executor.model.clone();
        o.reasoning_effort = a.executor.effort.clone();
        o.timeout = Duration::from_secs(u64::from(a.limits.timeout_s));
        let know = AppKnowledge { info: c.app_info.clone(), verified: vec![] };
        let hints = if c.hafa_hints {
            device_hints(c.screen.0, c.screen.1, bottom_shift)
        } else {
            generic_device_hints(c.screen.0, c.screen.1, bottom_shift)
        };
        let prompt = build_task_prompt(&a.case, &know, &hints);
        let warnings = rep.clone();
        let upload = {
            let (dir, rep, base, token, send) = (run_dir.clone(), rep.clone(), c.platform_http.clone(), c.node_token.clone(), send.clone());
            async move { upload_recordings(&dir, &rep, &base, &token, &*send).await }
        };
        let mut recorder = Recorder::start(&serial, &run_dir, adb, device.clone(), move |msg| warnings.recording_warn(msg), upload).await;
        rep.log("info", "执行员开始".into());
        let execution_dir = run_dir.clone();
        let execution = track_execution(
            &run_dir, &rep, &c.platform_http, &c.node_token,
            async {
                let summary = execute(o, prompt, execution_dir).await;
                recorder.stop().await;
                summary
            },
            &*send,
        ).await;
        let sum = &execution.summary;
        rep.log("info", format!("执行员结束：模型 {:?} 强度 {:?} 用时 {:.0}s 工具调用 {} 次", sum.model, sum.effort, sum.wall_s, sum.tool_calls));
        if let Some(f) = &secrets_file {
            let _ = std::fs::remove_file(f); // MCP 没来得及读也不留明文
        }
        let teardown = self.teardown(&serial).await;
        drop(device);
        recorder.wait_for_upload().await;
        if teardown != SetupResult::Ok && a.lease_id.is_some() && !sum.quota_exhausted {
            return rep.blocked(BlockedReason::Setup, "执行后恢复环境失败".into());
        }
        self.report_execution(&a, rep, &run_dir, execution, judge_opts).await;
    }

    async fn report_execution(&self, a: &RunAssign, rep: Reporter, run_dir: &Path, execution: Execution, judge_opts: ClaudeOpts) {
        let Execution { summary: sum, trace, uploaded } = execution;
        let cost = Some(json!({"model": sum.model, "effort": sum.effort, "wall_s": sum.wall_s, "tool_calls": sum.tool_calls, "tokens": sum.tokens.as_ref().map(|t| format!("{t:?}"))}));
        if sum.quota_exhausted {
            return rep.quota_exhausted(ModelSource::CodexExec);
        }
        // 规则 7：命令失败要把原始错误给用户看。Summary 里已经记着 CLI 的错误输出尾部和退出码，不能丢；
        // 没有错误输出就如实说没有，不编造原因。
        let code = sum.exit_code.map_or_else(|| "未知".to_string(), |c| c.to_string());
        let detail = match &sum.error {
            Some(error) => format!("原始错误（退出码 {code}）：{error}"),
            None => format!("没有错误输出，退出码 {code}"),
        };
        let Some(trace) = trace else {
            return rep.blocked(BlockedReason::Other, format!("执行员没有留下轨迹。（{detail}）"));
        };
        let result = trace.result.as_deref().unwrap_or_default();
        let reason = trace.reason.unwrap_or_default();
        let verdict = match result {
            "passed" => Verdict::Passed,
            "failed" => Verdict::Failed,
            "uncertain" => Verdict::Uncertain,
            // 执行员自己标「受阻」：如果它其实走完了步骤、留下了证据，只是对证据没把握（如图标被编号遮住），
            // 这不是链路 / 环境故障，应交给独立 Judge 看原图判定；只有没留下任何步骤、或被看门狗判卡住 / 超时才算受阻
            "blocked" if !trace.steps.is_empty() && !sum.stalled && !sum.timed_out => Verdict::Uncertain,
            "blocked" => return rep.blocked(BlockedReason::Stuck, format!("执行员判定受阻：{reason}")),
            _ => {
                return rep.blocked(
                    if sum.stalled { BlockedReason::Stuck } else { BlockedReason::Other },
                    format!("执行员没有给出结论。{reason}（{detail}）"),
                );
            }
        };
        rep.executor_done(verdict, reason.clone(), cost);

        // 3. 判定员按角色配置复核本次轨迹。
        // 有其他判定员：所有判定员同时评审同一份证据，各自出结论；单判定员走下面原来的流程，行为不变。
        if !a.extra_judges.is_empty() {
            return self.judge_with_several(a, &rep, run_dir, result, &reason, judge_opts, &uploaded).await;
        }
        let judge_source = a.judge.source;
        if !matches!(judge_source, ModelSource::ClaudeCli | ModelSource::Agy) || self.0.evidence_bin.as_os_str().is_empty() {
            if !matches!(judge_source, ModelSource::ClaudeCli | ModelSource::Agy) {
                rep.log("warn", format!("判定员来源 {judge_source:?} 还没有实现，本次不判定，转待人工确认"));
            }
            return rep.judge_done(JudgeStatus::NotRun, FinalResult::Uncertain, vec![]);
        }
        let mut outcome = if judge_source == ModelSource::Agy {
            // agy 的模型名里已带推理强度，不再单独传 effort
            let opts = agent::agy_cli::AgyOpts { model: a.judge.model.clone(), ..Default::default() };
            run_judge_agy(&opts, &a.case, result, &reason, run_dir, &self.0.evidence_bin).await
        } else {
            let opts = ClaudeOpts {
                model: a.judge.model.clone(),
                effort: a.judge.effort.clone(),
                cwd: Some(run_dir.to_path_buf()),
                ..judge_opts
            };
            run_judge(&opts, &a.case, result, &reason, run_dir, &self.0.evidence_bin).await
        };
        for item in &mut outcome.items {
            let cited = item.file_refs.len();
            // Judge 已核验步骤文字的引用，节点通过步骤消息上报这些步骤。
            item.file_refs.retain(|reference| {
                reference.starts_with("step:") || uploaded.contains(reference)
            });
            if item.file_refs.len() < cited {
                item.evidence.push_str(if item.file_refs.is_empty() { "（引用的截图上传失败）" } else { "（部分截图上传失败）" });
            }
        }
        let cost = outcome.cost_usd.map_or_else(|| "未返回".to_owned(), |usd| format!("{usd:.6} 美元"));
        rep.log("info", format!("判定结束，用时 {:.1} 秒，折算费用 {cost}", outcome.wall_s));
        if outcome.status != JudgeStatus::Completed {
            rep.log("warn", outcome.reason);
        }
        rep.judge_done(outcome.status, outcome.final_result, outcome.items);
        if outcome.quota_exhausted {
            rep.quota_exhausted(judge_source);
        }
    }

    /// 一个判定员评审一次。来源还没实现、或没有证据工具时，如实返回「没运行」（待人工确认），不编造结论。
    async fn judge_one(&self, role: &RoleModel, a: &RunAssign, result: &str, reason: &str, run_dir: &Path, judge_opts: &ClaudeOpts, uploaded: &std::collections::HashSet<String>) -> agent::judge::JudgeOutcome {
        let not_run = |why: String| agent::judge::JudgeOutcome {
            status: JudgeStatus::NotRun,
            final_result: FinalResult::Uncertain,
            items: vec![],
            reason: why,
            disagreement: String::new(),
            quota_exhausted: false,
            cost_usd: None,
            wall_s: 0.0,
            raw: String::new(),
        };
        if !matches!(role.source, ModelSource::ClaudeCli | ModelSource::Agy) {
            return not_run(format!("判定员来源 {:?} 还没有实现，这个判定员没有评审，转待人工确认", role.source));
        }
        if self.0.evidence_bin.as_os_str().is_empty() {
            return not_run("没有证据工具，这个判定员没有评审，转待人工确认".into());
        }
        let mut outcome = if role.source == ModelSource::Agy {
            let opts = agent::agy_cli::AgyOpts { model: role.model.clone(), ..Default::default() };
            run_judge_agy(&opts, &a.case, result, reason, run_dir, &self.0.evidence_bin).await
        } else {
            let opts = ClaudeOpts { model: role.model.clone(), effort: role.effort.clone(), cwd: Some(run_dir.to_path_buf()), ..judge_opts.clone() };
            run_judge(&opts, &a.case, result, reason, run_dir, &self.0.evidence_bin).await
        };
        for item in &mut outcome.items {
            let cited = item.file_refs.len();
            item.file_refs.retain(|reference| reference.starts_with("step:") || uploaded.contains(reference));
            if item.file_refs.len() < cited {
                item.evidence.push_str(if item.file_refs.is_empty() { "（引用的截图上传失败）" } else { "（部分截图上传失败）" });
            }
        }
        outcome
    }

    /// 多个判定员：**同时**评审同一份证据（互不知道对方的结论），每个人的结果单独上报、单独记录；
    /// 最终结果由服务端汇总（都完成且一致才采用，否则待人工确认）。这里也按同样的规则算一份给 `judge_done`，服务端以自己的为准。
    #[allow(clippy::too_many_arguments)]
    async fn judge_with_several(&self, a: &RunAssign, rep: &Reporter, run_dir: &Path, result: &str, reason: &str, judge_opts: ClaudeOpts, uploaded: &std::collections::HashSet<String>) {
        let roles: Vec<&RoleModel> = std::iter::once(&a.judge).chain(a.extra_judges.iter()).collect();
        rep.log("info", format!("{} 个判定员同时评审", roles.len()));
        let outcomes = futures_util::future::join_all(roles.iter().map(|role| self.judge_one(role, a, result, reason, run_dir, &judge_opts, uploaded))).await;
        let mut opinions = Vec::new();
        for (seq, (role, outcome)) in roles.iter().zip(&outcomes).enumerate() {
            let cost = outcome.cost_usd.map_or_else(|| "未返回".to_owned(), |usd| format!("{usd:.6} 美元"));
            rep.log("info", format!("判定员 {} 评审结束：{:?}，用时 {:.1} 秒，折算费用 {cost}", seq + 1, outcome.final_result, outcome.wall_s));
            if outcome.status != JudgeStatus::Completed {
                rep.log("warn", format!("判定员 {} 没有完成评审：{}", seq + 1, outcome.reason));
            }
            opinions.push(JudgeOpinion {
                seq: seq as u32,
                judge: (*role).clone(),
                judge_status: outcome.status,
                final_result: outcome.final_result,
                items: if outcome.status == JudgeStatus::Completed { outcome.items.clone() } else { vec![] },
            });
        }
        let (status, final_result) = protocol::consensus(&opinions.iter().map(|o| (o.judge_status, o.final_result)).collect::<Vec<_>>());
        let primary_items = opinions.first().map(|o| o.items.clone()).unwrap_or_default();
        rep.judge_opinions(opinions);
        rep.judge_done(status, final_result, primary_items);
        for (role, outcome) in roles.iter().zip(&outcomes) {
            if outcome.quota_exhausted {
                rep.quota_exhausted(role.source);
            }
        }
    }
}

#[cfg(test)]
#[path = "real_tests.rs"]
mod tests;
