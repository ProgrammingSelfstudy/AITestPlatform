//! 节点 ↔ 服务端通信协议（v1）。这里的类型是协议的唯一来源，说明见 `docs/节点协议.md`。
//!
//! 约定：
//! - 信封固定为 `{v, id, ts, type, payload}`；
//! - 收到未知 `type` 不报错：用 [`parse`] 解析，得到 `Parsed::UnknownType`，调用方记日志后忽略，保证向前兼容；
//!   已知 `type` 但内容格式不对仍然报错；
//! - 未知字段一律容忍（不使用 `deny_unknown_fields`）；
//! - 账号密码**不出现在任何协议类型里**：`RunAssign` 只带 `lease_id`，凭证走单独的 HTTPS 接口。

use chrono::{DateTime, Utc};
use serde::de::{DeserializeOwned, value::MapDeserializer};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod recording;

/// 当前协议版本。改字段含义或删字段才升版本，只加可选字段不升。
pub const PROTOCOL_VERSION: u32 = 1;

/// 默认心跳间隔（秒）。服务端在 `Welcome` 里下发，节点以下发值为准。
pub const DEFAULT_HEARTBEAT_S: u32 = 10;

/// 消息信封。`B` 是 `NodeMsg` 或 `ServerMsg`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope<B> {
    pub v: u32,
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub body: B,
}

impl<B> Envelope<B> {
    pub fn new(body: B) -> Self {
        Self { v: PROTOCOL_VERSION, id: Uuid::new_v4(), ts: Utc::now(), body }
    }
}

/// [`parse`] 的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed<B> {
    Known(Envelope<B>),
    /// 版本更新的对端发来了我们不认识的消息类型：记日志后忽略，不断开连接
    UnknownType { v: u32, id: Uuid, type_name: String },
}

#[derive(Deserialize)]
struct MessageTag {
    #[serde(rename = "type")]
    name: String,
}

#[derive(Debug, thiserror::Error)]
enum MessageTagError {
    #[error("未知消息类型")]
    UnknownVariant,
    #[error("消息类型解析失败")]
    Invalid,
}

impl serde::de::Error for MessageTagError {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Self::Invalid
    }

    fn unknown_variant(_: &str, _: &'static [&'static str]) -> Self {
        Self::UnknownVariant
    }
}

/// 解析一条消息。未知 `type` 返回 `UnknownType`，其他格式错误照常返回 `Err`。
pub fn parse<B: DeserializeOwned>(text: &str) -> Result<Parsed<B>, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let error = match Envelope::<B>::deserialize(&value) {
        Ok(env) => return Ok(Parsed::Known(env)),
        Err(error) => error,
    };
    let Ok(Envelope { v, id, body, .. }) = Envelope::<MessageTag>::deserialize(&value) else {
        return Err(error);
    };
    let type_name = body.name;
    // 只传入顶层 type 来检查消息枚举。真实 payload 的解析错误原样返回。
    let tag = MapDeserializer::<_, MessageTagError>::new([("type", type_name.as_str())].into_iter());
    match B::deserialize(tag) {
        Err(MessageTagError::UnknownVariant) => Ok(Parsed::UnknownType { v, id, type_name }),
        _ => Err(error),
    }
}

// ---------------------------------------------------------------- 公共类型

/// 模型来源。除 `DeepSeek` 外都是本机会员登录，**只有 DeepSeek 允许 API Key**（规则 1）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    /// `claude` 命令行（本机 Claude 会员登录）
    ClaudeCli,
    /// `codex exec`（本机 ChatGPT 登录）
    CodexExec,
    /// `agy` 命令行（本机 Gemini 个人账号登录）
    Agy,
    /// DeepSeek API（唯一允许 API Key 的来源，Key 只在发起人自己的节点本机）
    #[serde(rename = "deepseek")]
    DeepSeek,
}

/// 角色所属岗位。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoleSlot {
    Executor,
    Judge,
    /// 测试用例：根据需求文档和 UI 截图生成用例
    CaseWriter,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleDefault {
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleModelOption {
    pub model: String,
    /// 空表示没有单独的推理强度档位（强度已写进模型名）
    pub efforts: Vec<String>,
}

/// 一个角色 = 某个岗位可选的一个来源和它的模型。配置里**不含 Key**。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleConfig {
    pub id: String,
    pub slot: RoleSlot,
    pub name: String,
    pub source: ModelSource,
    pub default: RoleDefault,
    pub models: Vec<RoleModelOption>,
}

/// 检测结果。失败时 `detail` 是已脱敏的原始错误（不含 Key），要原样给用户看。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Unchecked,
    Checking,
    Ok,
    NotLoggedIn,
    QuotaExhausted,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleCheck {
    pub status: CheckStatus,
    pub detail: Option<String>,
    /// RFC3339 时间
    pub checked_at: Option<String>,
}

/// 节点上报给服务端的角色摘要。`key_configured` 只有布尔，Key 永远不上报。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleReport {
    #[serde(flatten)]
    pub config: RoleConfig,
    pub key_configured: bool,
    pub check: RoleCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RolesReport {
    pub roles: Vec<RoleReport>,
}

/// 服务端下发的整体角色配置。`api_key` 只在新设或更换 Key 时带；`None` 表示保持节点本机原来的。
/// 自定义 `Debug`：永远不把 Key 打进日志。
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleSet {
    #[serde(flatten)]
    pub config: RoleConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl std::fmt::Debug for RoleSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoleSet")
            .field("config", &self.config)
            .field("api_key", &self.api_key.as_ref().map(|_| "<已隐藏>"))
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetRoles {
    pub roles: Vec<RoleSet>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Caps {
    pub node_version: String,
    pub os: String,
    pub model_sources: Vec<ModelSourceCap>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSourceCap {
    pub source: ModelSource,
    /// 该来源是否支持看图（执行岗位必须支持）
    pub vision: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    Idle,
    Busy,
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfo {
    pub serial: String,
    pub platform: String,
    pub model: Option<String>,
    pub os_version: Option<String>,
    pub screen_w: Option<u32>,
    pub screen_h: Option<u32>,
    pub status: DeviceStatus,
}

/// 判定结果。证据不足用 `Uncertain`，**不能强行通过**。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    Failed,
    Uncertain,
}

/// run 的最终结果。`Blocked` 表示没跑完（链路、环境、卡住等），不是用例失败。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinalResult {
    Passed,
    Failed,
    Uncertain,
    Blocked,
}

/// 受阻原因，统计完成率时要区分链路/环境故障。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    Setup,
    Link,
    NodeLost,
    Stuck,
    Quota,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JudgeStatus {
    Completed,
    Failed,
    NotRun,
}

/// 需要人介入的类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HumanKind {
    /// 支付、删除账号、注销等高危页面，工具层直接拒绝
    Danger,
    Stuck,
    ManualStep,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HumanAction {
    Continue,
    Abort,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseExpected {
    pub idx: u32,
    pub text: String,
}

/// 下发给节点的用例。`data` 里的账号是占位符 `{{account.xxx}}`，不含明文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseSpec {
    pub code: String,
    pub title: String,
    pub preconditions: Vec<String>,
    pub steps: Vec<String>,
    pub expected: Vec<CaseExpected>,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleModel {
    pub source: ModelSource,
    /// 模型名来自角色配置，代码里不写死
    pub model: Option<String>,
    /// 推理强度（如 low / medium / high / xhigh / max）。**不指定时 Codex 会落到模型目录里的默认值，通常是 low**，
    /// 而且平台为了锁死工具会忽略用户自己的 Codex 配置，所以执行员的推理强度必须在这里显式给出。
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Limits {
    pub max_steps: u32,
    pub timeout_s: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JudgeItem {
    pub idx: u32,
    pub verdict: Verdict,
    pub evidence: String,
    pub file_refs: Vec<String>,
}

// ---------------------------------------------------------------- 节点 → 服务端

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    /// 注册时拿到的节点令牌
    pub token: String,
    /// 节点第一次连接后服务端在 `Welcome` 里下发的 id；重连时带上，服务端据此认出是同一个节点。首次连接为空
    #[serde(default)]
    pub node_id: Option<Uuid>,
    /// 节点身份密钥：首次连接时服务端在 `Welcome` 里下发，节点本地保存，之后每次重连都要带上。
    /// 服务端已给该节点发过密钥时，缺失或不符一律按 `unauthorized` 拒绝，共享令牌不能冒领别的节点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_secret: Option<String>,
    /// 节点主人的个人令牌。节点还没有属主时，服务端据此绑定；已有属主时必须与属主一致（缺省视为不变）。
    /// 「谁发起谁付额度」靠它判断任务发起人是不是节点属主。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_token: Option<String>,
    pub caps: Caps,
    pub devices: Vec<DeviceInfo>,
    /// 重连时本地仍在跑的 run
    #[serde(default)]
    pub running_runs: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Heartbeat {
    pub devices: Vec<DeviceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub run_id: Uuid,
    /// `(run_id, idx)` 幂等，重传安全
    pub idx: u32,
    /// 步骤开始的节点时间（epoch 毫秒）；旧节点不发送时为 0。
    #[serde(default)]
    pub at_ms: i64,
    pub tool: String,
    /// 已脱敏的参数
    pub args: serde_json::Value,
    /// 结果摘要
    pub result: serde_json::Value,
    pub file_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutorDone {
    pub run_id: Uuid,
    pub result: Verdict,
    pub reason: String,
    pub cost: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JudgeDone {
    pub run_id: Uuid,
    pub judge_status: JudgeStatus,
    /// Judge 没跑成时必须是 `Uncertain`，不得用执行员自评顶替
    pub final_result: FinalResult,
    pub items: Vec<JudgeItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NeedHuman {
    pub run_id: Uuid,
    pub kind: HumanKind,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunBlocked {
    pub run_id: Uuid,
    pub reason: BlockedReason,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotaExhausted {
    pub source: ModelSource,
    pub run_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Frame {
    pub run_id: Uuid,
    /// JPEG 的 base64
    pub jpeg_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogLine {
    pub level: String,
    /// 已脱敏
    pub msg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum NodeMsg {
    Hello(Hello),
    Heartbeat(Heartbeat),
    DeviceUpdate(Heartbeat),
    RunStarted { run_id: Uuid },
    Step(Step),
    ExecutorDone(ExecutorDone),
    JudgeDone(JudgeDone),
    NeedHuman(NeedHuman),
    RunBlocked(RunBlocked),
    QuotaExhausted(QuotaExhausted),
    Frame(Frame),
    Log(LogLine),
    /// 对服务端需要确认的消息（如 `run_assign`）的确认，`ack_id` 是被确认消息的 id
    Ack { ack_id: Uuid },
    /// 角色摘要：连上后、配置变更后、检测完成后上报。服务端只在内存里缓存，不落库
    RolesReport(RolesReport),
}

impl NodeMsg {
    pub fn tracked_run(&self) -> Option<Uuid> {
        match self {
            Self::RunStarted { run_id } => Some(*run_id),
            Self::Step(step) => Some(step.run_id),
            Self::ExecutorDone(done) => Some(done.run_id),
            Self::JudgeDone(done) => Some(done.run_id),
            Self::RunBlocked(blocked) => Some(blocked.run_id),
            Self::QuotaExhausted(exhausted) => exhausted.run_id,
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- 服务端 → 节点

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeDecision {
    Continue,
    Abort,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResumeItem {
    pub run_id: Uuid,
    pub decision: ResumeDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Welcome {
    pub node_id: Uuid,
    /// 只在服务端新发（或首次补发）密钥时出现；节点必须保存。`Debug` 会打印它，不要把 `Welcome` 写进日志。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_secret: Option<String>,
    pub heartbeat_s: u32,
    #[serde(default)]
    pub resume: Vec<ResumeItem>,
    #[serde(default)]
    pub ack_run_updates: bool,
}

/// 下发任务。**不含账号密码**：节点拿 `lease_id` 用节点令牌调本机 HTTP 接口取一次性凭证。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunAssign {
    pub run_id: Uuid,
    pub case: CaseSpec,
    pub device_serial: String,
    pub executor: RoleModel,
    pub judge: RoleModel,
    /// 账号租约。`None` 表示这次执行不需要账号（手机上已经是目标账号）；有账号时节点凭它取一次性凭证。
    /// 不用全零 id 占位：那会让「有租约」和「没租约」无法区分。
    #[serde(default)]
    pub lease_id: Option<Uuid>,
    /// 上一轮使用后环境未恢复，节点需要先执行收尾。
    #[serde(default)]
    pub needs_cleanup: bool,
    pub limits: Limits,
}

/// 仅用于节点的一次性取凭证接口。账号 ID 是登录态缓存的稳定标识。
#[derive(Serialize, Deserialize)]
pub struct LeaseCredentials {
    pub account_id: Uuid,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HumanResolved {
    pub run_id: Uuid,
    pub action: HumanAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProtoError {
    /// 如 `unauthorized`、`version_too_old`
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome(Welcome),
    RunAssign(RunAssign),
    RunCancel { run_id: Uuid },
    HumanResolved(HumanResolved),
    SubscribeFrames { run_id: Uuid },
    UnsubscribeFrames { run_id: Uuid },
    Ping,
    Error(ProtoError),
    Ack { ack_id: Uuid },
    /// 整体替换节点本机的角色配置
    SetRoles(SetRoles),
    /// 让节点真调一次模型，检测这个角色能不能用
    CheckRole { role_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caps() -> Caps {
        Caps {
            node_version: "0.1.0".into(),
            os: "macos".into(),
            model_sources: vec![ModelSourceCap { source: ModelSource::ClaudeCli, vision: true }],
        }
    }

    fn device() -> DeviceInfo {
        DeviceInfo {
            serial: "1263040493010797".into(),
            platform: "android".into(),
            model: Some("X6531".into()),
            os_version: None,
            screen_w: Some(720),
            screen_h: Some(1600),
            status: DeviceStatus::Idle,
        }
    }

    fn roundtrip<B>(body: B)
    where
        B: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug + Clone,
    {
        let env = Envelope::new(body);
        let text = serde_json::to_string(&env).unwrap();
        let back: Envelope<B> = serde_json::from_str(&text).unwrap();
        assert_eq!(env, back);
    }

    #[test]
    fn 信封字段齐全() {
        let env = Envelope::new(NodeMsg::RunStarted { run_id: Uuid::nil() });
        let v: serde_json::Value = serde_json::to_value(&env).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "run_started");
        assert!(v["id"].is_string() && v["ts"].is_string() && v["payload"].is_object());
    }

    #[test]
    fn 节点消息往返() {
        let run_id = Uuid::new_v4();
        roundtrip(NodeMsg::Hello(Hello { token: "t".into(), node_id: Some(Uuid::new_v4()), node_secret: Some("s".into()), user_token: Some("u".into()), caps: caps(), devices: vec![device()], running_runs: vec![run_id] }));
        roundtrip(NodeMsg::Heartbeat(Heartbeat { devices: vec![device()] }));
        roundtrip(NodeMsg::RunStarted { run_id });
        roundtrip(NodeMsg::Step(Step {
            run_id, idx: 3, at_ms: 1_700_000_000_000, tool: "tap_element".into(),
            args: json!({"desc": "Add Music"}), result: json!({"ok": true}), file_ref: Some("shots/03.png".into()),
        }));
        roundtrip(NodeMsg::ExecutorDone(ExecutorDone { run_id, result: Verdict::Uncertain, reason: "r".into(), cost: None }));
        roundtrip(NodeMsg::JudgeDone(JudgeDone {
            run_id, judge_status: JudgeStatus::Failed, final_result: FinalResult::Uncertain, items: vec![],
        }));
        roundtrip(NodeMsg::NeedHuman(NeedHuman { run_id, kind: HumanKind::Danger, reason: "支付页".into() }));
        roundtrip(NodeMsg::RunBlocked(RunBlocked { run_id, reason: BlockedReason::Link, detail: "reconnect_loop".into() }));
        roundtrip(NodeMsg::QuotaExhausted(QuotaExhausted { source: ModelSource::CodexExec, run_id: Some(run_id) }));
        roundtrip(NodeMsg::Ack { ack_id: Uuid::new_v4() });
    }

    #[test]
    fn 旧节点的步骤消息没有开始时间仍可解析() {
        let step: Step = serde_json::from_value(json!({
            "run_id": Uuid::nil(), "idx": 1, "tool": "tap", "args": {}, "result": {}
        })).unwrap();
        assert_eq!(step.at_ms, 0);
    }

    #[test]
    fn 服务端消息往返() {
        let run_id = Uuid::new_v4();
        roundtrip(ServerMsg::Welcome(Welcome {
            node_id: Uuid::new_v4(), node_secret: Some("s".into()), heartbeat_s: DEFAULT_HEARTBEAT_S,
            resume: vec![ResumeItem { run_id, decision: ResumeDecision::Continue }],
            ack_run_updates: true,
        }));
        roundtrip(ServerMsg::RunAssign(RunAssign {
            run_id,
            case: CaseSpec {
                code: "TC-04".into(), title: "t".into(), preconditions: vec![], steps: vec!["s".into()],
                expected: vec![CaseExpected { idx: 1, text: "e".into() }], data: json!({"user": "{{account.username}}"}),
            },
            device_serial: "1263040493010797".into(),
            executor: RoleModel { source: ModelSource::CodexExec, model: None, effort: None },
            judge: RoleModel { source: ModelSource::ClaudeCli, model: None, effort: None },
            lease_id: Some(Uuid::new_v4()),
            needs_cleanup: true,
            limits: Limits { max_steps: 60, timeout_s: 900 },
        }));
        roundtrip(ServerMsg::RunCancel { run_id });
        roundtrip(ServerMsg::HumanResolved(HumanResolved { run_id, action: HumanAction::Abort }));
        roundtrip(ServerMsg::Ack { ack_id: run_id });
        roundtrip(ServerMsg::Error(ProtoError { code: "run_rejected".into(), message: "m".into(), ack_id: Some(run_id) }));
        roundtrip(ServerMsg::Ping);
    }

    #[test]
    fn 未知类型不报错() {
        let text = r#"{"v":1,"id":"00000000-0000-0000-0000-000000000000","ts":"2026-11-20T10:00:00Z","type":"from_the_future","payload":{"x":1}}"#;
        match parse::<ServerMsg>(text).unwrap() {
            Parsed::UnknownType { type_name, v, .. } => {
                assert_eq!(type_name, "from_the_future");
                assert_eq!(v, 1);
            }
            other => panic!("应当识别为未知类型，实际：{other:?}"),
        }
    }

    #[test]
    fn 已知类型但格式错仍然报错() {
        // run_cancel 缺 run_id：不能被当成「未知类型」悄悄忽略，否则会吞掉真正的协议错误
        let text = r#"{"v":1,"id":"00000000-0000-0000-0000-000000000000","ts":"2026-11-20T10:00:00Z","type":"run_cancel","payload":{}}"#;
        assert!(parse::<ServerMsg>(text).is_err());
    }

    #[test]
    fn 欢迎消息的非法恢复决定返回解析错误() {
        let text = json!({
            "v": 1,
            "id": Uuid::nil(),
            "ts": "2026-11-20T10:00:00Z",
            "type": "welcome",
            "payload": {
                "node_id": Uuid::nil(),
                "heartbeat_s": DEFAULT_HEARTBEAT_S,
                "resume": [{"run_id": Uuid::nil(), "decision": "welcome"}]
            }
        }).to_string();
        assert!(parse::<ServerMsg>(&text).is_err());
    }

    #[test]
    fn 旧版欢迎消息省略恢复列表时默认为空() {
        let text = json!({
            "v": 1,
            "id": Uuid::nil(),
            "ts": "2026-11-20T10:00:00Z",
            "type": "welcome",
            "payload": {"node_id": Uuid::nil(), "heartbeat_s": DEFAULT_HEARTBEAT_S}
        }).to_string();
        let Parsed::Known(Envelope { body: ServerMsg::Welcome(welcome), .. }) =
            parse::<ServerMsg>(&text).unwrap()
        else {
            panic!("应解析为欢迎消息");
        };
        assert!(welcome.resume.is_empty());
    }

    #[test]
    fn parse解析已知类型() {
        let env = Envelope::new(NodeMsg::RunStarted { run_id: Uuid::nil() });
        let text = serde_json::to_string(&env).unwrap();
        assert_eq!(parse::<NodeMsg>(&text).unwrap(), Parsed::Known(env));
    }

    #[test]
    fn 未知字段容忍() {
        let text = r#"{"v":1,"id":"00000000-0000-0000-0000-000000000000","ts":"2026-11-20T10:00:00Z","type":"run_cancel","payload":{"run_id":"00000000-0000-0000-0000-000000000000","extra":"新增字段"}}"#;
        let env: Envelope<ServerMsg> = serde_json::from_str(text).unwrap();
        assert_eq!(env.body, ServerMsg::RunCancel { run_id: Uuid::nil() });
    }

    #[test]
    fn 下发任务里没有密码字段() {
        let assign = RunAssign {
            run_id: Uuid::nil(),
            case: CaseSpec {
                code: "TC-01".into(), title: "t".into(), preconditions: vec![], steps: vec![], expected: vec![],
                data: json!({"account": "{{account.username}}"}),
            },
            device_serial: "x".into(),
            executor: RoleModel { source: ModelSource::ClaudeCli, model: None, effort: None },
            judge: RoleModel { source: ModelSource::ClaudeCli, model: None, effort: None },
            lease_id: None,
            needs_cleanup: false,
            limits: Limits { max_steps: 1, timeout_s: 1 },
        };
        let text = serde_json::to_string(&assign).unwrap().to_lowercase();
        assert!(!text.contains("password") && !text.contains("passwd"), "协议里不能出现密码字段：{text}");
        assert!(!text.contains("\"lease_id\":\"0000"), "没有租约时不能用全零 id 占位");
    }

    #[test]
    fn 旧派发消息默认不要求清理() {
        let assign: RunAssign = serde_json::from_value(json!({
            "run_id": Uuid::nil(),
            "case": {"code": "TC-01", "title": "标题", "preconditions": [], "steps": [], "expected": [], "data": {}},
            "device_serial": "测试设备",
            "executor": {"source": "codex_exec"},
            "judge": {"source": "claude_cli"},
            "limits": {"max_steps": 1, "timeout_s": 2}
        })).unwrap();
        assert!(assign.lease_id.is_none());
        assert!(!assign.needs_cleanup);
    }

    #[test]
    fn judge没跑成时最终结果是待确认() {
        // 固定这个语义：Judge 失败时 final_result 只能是 Uncertain，不能由调用方改成 Passed。
        let done = JudgeDone { run_id: Uuid::nil(), judge_status: JudgeStatus::Failed, final_result: FinalResult::Uncertain, items: vec![] };
        assert_eq!(done.final_result, FinalResult::Uncertain);
    }

    fn role(id: &str) -> RoleConfig {
        RoleConfig {
            id: id.into(),
            slot: RoleSlot::Judge,
            name: "判定员 · 测试".into(),
            source: ModelSource::Agy,
            default: RoleDefault { model: "m".into(), effort: None },
            models: vec![RoleModelOption { model: "m".into(), efforts: vec![] }],
        }
    }

    #[test]
    fn 角色消息往返并且摘要里没有key字段() {
        roundtrip(ServerMsg::SetRoles(SetRoles { roles: vec![RoleSet { config: role("a"), api_key: Some("sk-密钥".into()) }] }));
        roundtrip(ServerMsg::CheckRole { role_id: "a".into() });
        let report = RolesReport {
            roles: vec![RoleReport { config: role("a"), key_configured: true, check: RoleCheck { status: CheckStatus::NotLoggedIn, detail: Some("未登录".into()), checked_at: None } }],
        };
        roundtrip(NodeMsg::RolesReport(report.clone()));
        let text = serde_json::to_string(&report).unwrap().to_lowercase();
        assert!(!text.contains("api_key") && !text.contains("sk-"), "上报的角色摘要里不能有 Key：{text}");
        assert!(text.contains("\"key_configured\":true"));
    }

    #[test]
    fn 角色的调试输出不泄露key() {
        let set = RoleSet { config: role("a"), api_key: Some("sk-绝不能出现".into()) };
        let shown = format!("{set:?}");
        assert!(!shown.contains("sk-绝不能出现"), "Debug 输出泄露了 Key：{shown}");
        let all = format!("{:?}", ServerMsg::SetRoles(SetRoles { roles: vec![set] }));
        assert!(!all.contains("sk-绝不能出现"), "消息的 Debug 输出泄露了 Key：{all}");
    }
}
