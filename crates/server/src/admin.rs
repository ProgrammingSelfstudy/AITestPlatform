//! 管理员聊天：读需求文档和各模块现状，和人聊天，**只提议，人点确认才执行**。
//!
//! - 对话和提议按人存（每个人自己的）；回答由**这个人自己节点**上的管理员角色生成，用的是他自己的模型和额度（规则 9）。
//! - 管理员给的提议先由这里**按库里的数据核对**（模块存不存在、用例是否真的存在且已批准），核对不过的丢掉并说明；
//!   通过的存成 `pending` 卡片。**管理员自己什么也不会做，也不能批准用例**。
//! - 人点确认时再核对一遍（防止过期），然后服务端用人的身份、在人的节点上执行：
//!   `create_module` 建模块；`run_tests` 对已批准的用例发起测试（建任务，用节点上检测通过的执行员和判定员角色）；
//!   `generate_cases` 需要 UI 链接和截图，管理员拿不到，所以确认后只是告诉前端打开预填好的表单。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use protocol::{AdminAnswer, AdminAsk, AdminProposal, ChatLine, ModelSource, RoleModel, RoleSlot, ServerMsg};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::api::ApiState;
use crate::internal;
use crate::runs::{self, NewTask};

/// 项目固定只有一个。
pub const PROJECT: &str = "hafa";
const MAX_MESSAGE_CHARS: usize = 4000;
const MAX_DOC_CHARS_IN_CONTEXT: usize = 24_000;
const HISTORY_LINES: i64 = 20;
/// 问出去超过这么久没有回答，当作没回答（节点断了、模型卡住）。
pub const ASK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 还没回答的提问：谁问的、问的哪个节点、什么时候问的。只在内存里。
pub struct PendingAsk {
    pub user: Uuid,
    pub node: Uuid,
    pub at: Instant,
}

pub type PendingAsks = std::sync::Mutex<HashMap<Uuid, PendingAsk>>;

type ApiError = (StatusCode, String);

fn bad(message: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}

// ---------------------------------------------------------------- 项目现状（给管理员读的数据）

/// 整理给管理员的项目现状：最新的需求文档、各模块和它们的用例状态。对管理员来说是数据，不是指令。
pub async fn project_context(pool: &PgPool, project: &str) -> Result<String, sqlx::Error> {
    let mut out = String::new();
    let doc: Option<String> = sqlx::query_scalar(
        "select d.content from project_docs d join projects p on p.id = d.project_id where p.name = $1 order by d.created_at desc limit 1",
    )
    .bind(project)
    .fetch_optional(pool)
    .await?;
    out.push_str("【需求文档（最新）】\n");
    match doc {
        Some(text) => {
            let shown: String = text.chars().take(MAX_DOC_CHARS_IN_CONTEXT).collect();
            out.push_str(&shown);
            if text.chars().count() > MAX_DOC_CHARS_IN_CONTEXT {
                out.push_str("\n…（文档太长，后面的没有给你）");
            }
        }
        None => out.push_str("（还没有提交过需求文档）"),
    }
    out.push_str("\n\n【模块】\n");
    let modules = sqlx::query(
        "select m.id, m.name,
                (select count(*) from test_cases c where c.module_id = m.id) as total,
                (select count(*) from test_cases c where c.module_id = m.id and c.review_status = 'approved') as approved,
                (select count(*) from test_cases c where c.module_id = m.id and c.review_status = 'needs_human') as needs_human,
                (select count(*) from runs x join test_cases c on c.id = x.case_id where c.module_id = m.id and x.status in ('queued','assigned','running','judging')) as active
         from modules m join projects p on p.id = m.project_id where p.name = $1 order by m.created_at, m.name",
    )
    .bind(project)
    .fetch_all(pool)
    .await?;
    if modules.is_empty() {
        out.push_str("（还没有模块）\n");
    }
    for m in &modules {
        let id: Uuid = m.get("id");
        out.push_str(&format!(
            "- 模块「{}」：用例 {} 条，已批准 {} 条，需要人工确认 {} 条，正在测试的 run {} 个\n",
            m.get::<String, _>("name"),
            m.get::<i64, _>("total"),
            m.get::<i64, _>("approved"),
            m.get::<i64, _>("needs_human"),
            m.get::<i64, _>("active"),
        ));
        let approved = sqlx::query("select code, title from test_cases where module_id = $1 and review_status = 'approved' order by code limit 40").bind(id).fetch_all(pool).await?;
        for c in &approved {
            out.push_str(&format!("    已批准用例 {}：{}\n", c.get::<String, _>("code"), c.get::<String, _>("title")));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- 提议的核对

/// 一条提议核对的结果：通过（可能改写了参数，比如去掉不存在的用例）或丢掉（附原因）。
pub enum Checked {
    Keep(serde_json::Value),
    Drop(String),
}

fn name_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 按库里的数据核对一条提议。管理员说的模块、用例编号都不能信：必须真的存在，`run_tests` 的用例还必须是已批准的、可执行的。
pub async fn check_proposal(pool: &PgPool, project: &str, p: &AdminProposal) -> Result<Checked, sqlx::Error> {
    let module_id = |name: String| async move {
        sqlx::query_scalar::<_, Uuid>("select m.id from modules m join projects p on p.id = m.project_id where p.name = $1 and m.name = $2").bind(project).bind(name).fetch_optional(pool).await
    };
    Ok(match p.kind.as_str() {
        "create_module" => {
            let Some(name) = name_arg(&p.args, "name").and_then(|n| crate::modules::check_module_name(&n).ok()) else { return Ok(Checked::Drop("新建模块：模块名不合格".into())) };
            if module_id(name.clone()).await?.is_some() {
                return Ok(Checked::Drop(format!("新建模块「{name}」：这个模块已经存在")));
            }
            Checked::Keep(serde_json::json!({ "name": name }))
        }
        "generate_cases" => {
            let Some(name) = name_arg(&p.args, "module").and_then(|n| crate::modules::check_module_name(&n).ok()) else { return Ok(Checked::Drop("生成用例：模块名不合格".into())) };
            // 模块可以还不存在（新需求先建模块再生成，或者直接在表单里新建）
            Checked::Keep(serde_json::json!({ "module": name }))
        }
        "run_tests" => {
            let Some(name) = name_arg(&p.args, "module") else { return Ok(Checked::Drop("发起测试：没有写模块".into())) };
            let Some(mid) = module_id(name.clone()).await? else { return Ok(Checked::Drop(format!("发起测试：没有模块「{name}」"))) };
            let wanted: Vec<String> = p.args.get("case_codes").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_string())).collect()).unwrap_or_default();
            let ok: Vec<String> = sqlx::query_scalar("select code from test_cases where module_id = $1 and review_status = 'approved' and status = 'active' and code = any($2) order by code")
                .bind(mid)
                .bind(&wanted)
                .fetch_all(pool)
                .await?;
            if ok.is_empty() {
                return Ok(Checked::Drop(format!("发起测试：模块「{name}」里没有提到的已批准用例（管理员给的编号不存在或还没批准）")));
            }
            Checked::Keep(serde_json::json!({ "module": name, "case_codes": ok }))
        }
        other => Checked::Drop(format!("不认识的提议类型 {other}")),
    })
}

// ---------------------------------------------------------------- 对话

#[derive(Serialize)]
pub struct ProposalView {
    pub id: Uuid,
    pub kind: String,
    pub args: serde_json::Value,
    pub why: String,
    pub status: String,
    pub result: String,
}

#[derive(Serialize)]
pub struct MessageView {
    pub id: Uuid,
    pub author: String,
    pub text: String,
    pub is_error: bool,
    pub created_at: DateTime<Utc>,
    pub proposals: Vec<ProposalView>,
}

#[derive(Serialize)]
pub struct Chat {
    /// 正在等管理员回答（问出去还没回来）：问题编号和开始时间
    pub pending: Option<PendingView>,
    pub messages: Vec<MessageView>,
}

#[derive(Serialize)]
pub struct PendingView {
    pub ask_id: Uuid,
    pub since: DateTime<Utc>,
}

pub async fn load_chat(pool: &PgPool, user: Uuid) -> Result<Vec<MessageView>, sqlx::Error> {
    let rows = sqlx::query("select id, author, text, is_error, created_at from (select * from admin_messages where user_id = $1 order by created_at desc limit 100) m order by created_at")
        .bind(user)
        .fetch_all(pool)
        .await?;
    let props = sqlx::query("select p.id, p.message_id, p.kind, p.args, p.why, p.status, p.result from admin_proposals p where p.user_id = $1 order by p.created_at")
        .bind(user)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let id: Uuid = r.get("id");
            MessageView {
                id,
                author: r.get("author"),
                text: r.get("text"),
                is_error: r.get("is_error"),
                created_at: r.get("created_at"),
                proposals: props
                    .iter()
                    .filter(|p| p.get::<Uuid, _>("message_id") == id)
                    .map(|p| ProposalView { id: p.get("id"), kind: p.get("kind"), args: p.get("args"), why: p.get("why"), status: p.get("status"), result: p.get("result") })
                    .collect(),
            }
        })
        .collect())
}

async fn history(pool: &PgPool, user: Uuid) -> Result<Vec<ChatLine>, sqlx::Error> {
    let rows = sqlx::query("select author, text from (select author, text, created_at from admin_messages where user_id = $1 and not is_error order by created_at desc limit $2) m order by created_at")
        .bind(user)
        .bind(HISTORY_LINES)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| ChatLine { author: r.get("author"), text: r.get("text") }).collect())
}

/// 节点把管理员的回答送回来：核对提议，把回答和通过核对的提议存下来。回答对不上任何等待中的提问（过期、别的节点冒送）就忽略。
pub async fn on_answer(pool: &PgPool, pending: &PendingAsks, node: Uuid, answer: AdminAnswer) -> Result<(), sqlx::Error> {
    let ask = {
        let mut map = pending.lock().unwrap();
        match map.get(&answer.ask_id) {
            Some(a) if a.node == node => map.remove(&answer.ask_id),
            _ => None,
        }
    };
    let Some(ask) = ask else {
        tracing::warn!(%node, "收到没有对应提问的管理员回答，忽略");
        return Ok(());
    };
    if !answer.ok {
        let reason: String = answer.error.chars().take(1500).collect();
        sqlx::query("insert into admin_messages (user_id, author, text, is_error) values ($1, 'admin', $2, true)").bind(ask.user).bind(format!("管理员没有回答成功：{reason}")).execute(pool).await?;
        return Ok(());
    }
    let mut kept = vec![];
    let mut notes = vec![];
    for p in &answer.proposals {
        match check_proposal(pool, PROJECT, p).await? {
            Checked::Keep(args) => kept.push((p, args)),
            Checked::Drop(why) => notes.push(why),
        }
    }
    let mut text = answer.reply.trim().to_string();
    if text.is_empty() {
        text = "（管理员没有说话，只给了提议）".into();
    }
    if !notes.is_empty() {
        text.push_str(&format!("\n\n（有 {} 条提议核对不通过，已丢掉：{}）", notes.len(), notes.join("；")));
    }
    let text: String = text.chars().take(30000).collect();
    let mut tx = pool.begin().await?;
    let message: Uuid = sqlx::query_scalar("insert into admin_messages (user_id, author, text) values ($1, 'admin', $2) returning id").bind(ask.user).bind(&text).fetch_one(&mut *tx).await?;
    for (p, args) in kept {
        sqlx::query("insert into admin_proposals (message_id, user_id, node_id, kind, args, why) values ($1,$2,$3,$4,$5,$6)")
            .bind(message)
            .bind(ask.user)
            .bind(ask.node)
            .bind(&p.kind)
            .bind(args)
            .bind(p.why.chars().take(600).collect::<String>())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

/// 超时没回答的提问转成一条错误消息，免得页面一直转圈。
async fn sweep_stale(pool: &PgPool, pending: &PendingAsks) -> Result<(), sqlx::Error> {
    let stale: Vec<PendingAsk> = {
        let mut map = pending.lock().unwrap();
        let keys: Vec<Uuid> = map.iter().filter(|(_, a)| a.at.elapsed() > ASK_TIMEOUT).map(|(k, _)| *k).collect();
        keys.into_iter().filter_map(|k| map.remove(&k)).collect()
    };
    for ask in stale {
        sqlx::query("insert into admin_messages (user_id, author, text, is_error) values ($1, 'admin', '管理员没有回答成功：超过 10 分钟没有回音（节点可能断线了，或模型卡住了）', true)").bind(ask.user).execute(pool).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------- HTTP

async fn user_of(state: &ApiState, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    crate::users::browser_user(state.app.pool(), headers)
        .await
        .map_err(|e| internal("校验用户失败", e))?
        .ok_or((StatusCode::UNAUTHORIZED, crate::users::NEED_NAME_MESSAGE.to_string()))
}

pub(crate) async fn api_chat(State(state): State<ApiState>, headers: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    let user = user_of(&state, &headers).await?;
    let pool = state.app.pool();
    sweep_stale(pool, state.app.admin_asks()).await.map_err(|e| internal("整理超时提问失败", e))?;
    let messages = load_chat(pool, user).await.map_err(|e| internal("读取对话失败", e))?;
    let pending = state.app.admin_asks().lock().unwrap().iter().filter(|(_, a)| a.user == user).map(|(id, a)| PendingView { ask_id: *id, since: Utc::now() - chrono::Duration::from_std(a.at.elapsed()).unwrap_or_default() }).next();
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(Chat { pending, messages })))
}

#[derive(Deserialize)]
pub(crate) struct SendInput {
    message: String,
    /// 用哪个节点上的管理员回答；不填用你名下第一个在线、管理员角色检测通过的节点
    #[serde(default)]
    node_id: Option<Uuid>,
}

pub(crate) async fn api_send(State(state): State<ApiState>, headers: HeaderMap, body: Result<Json<SendInput>, JsonRejection>) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let Json(input) = body.map_err(|_| bad("请求格式无效"))?;
    let user = user_of(&state, &headers).await?;
    let text = input.message.trim();
    if text.is_empty() || text.chars().count() > MAX_MESSAGE_CHARS {
        return Err(bad(format!("消息不能为空，也不超过 {MAX_MESSAGE_CHARS} 个字")));
    }
    let pool = state.app.pool();
    if state.app.admin_asks().lock().unwrap().values().any(|a| a.user == user) {
        return Err((StatusCode::CONFLICT, "管理员还在回答上一句，等它回完再说".into()));
    }
    // 找一个能回答的节点：你的、在线的、管理员角色检测通过的。用的是你自己的模型和额度
    let owned: Vec<Uuid> = sqlx::query_scalar("select id from nodes where owner_user_id = $1 order by id").bind(user).fetch_all(pool).await.map_err(|e| internal("查询节点失败", e))?;
    let mut chosen: Option<(Uuid, String)> = None;
    let mut why_not: Vec<String> = vec![];
    for node in owned.iter().filter(|n| input.node_id.is_none_or(|want| want == **n)) {
        if !state.app.node_connected(*node).await {
            why_not.push(format!("节点 {} 不在线", &node.to_string()[..8]));
            continue;
        }
        let planner = state.app.roles_report(*node).and_then(|r| r.roles.into_iter().find(|r| r.config.slot == RoleSlot::Planner && r.check.status == protocol::CheckStatus::Ok));
        match planner {
            Some(r) => {
                chosen = Some((*node, r.config.id));
                break;
            }
            None => why_not.push(format!("节点 {} 没有检测通过的「管理员」角色", &node.to_string()[..8])),
        }
    }
    let Some((node, role_id)) = chosen else {
        let detail = if owned.is_empty() { "你名下还没有节点（启动节点时设 NODE_USER_NAME 为你的名字）".to_string() } else { why_not.join("；") };
        return Err(bad(format!("没有能回答的节点：{detail}。请先在「角色与模型」页给你的节点配一个「管理员」角色并检测通过")));
    };
    let context = project_context(pool, PROJECT).await.map_err(|e| internal("整理项目现状失败", e))?;
    let hist = history(pool, user).await.map_err(|e| internal("读取对话失败", e))?;
    let message_id: Uuid = sqlx::query_scalar("insert into admin_messages (user_id, author, text) values ($1, 'user', $2) returning id").bind(user).bind(text).fetch_one(pool).await.map_err(|e| internal("保存消息失败", e))?;
    let ask_id = Uuid::new_v4();
    state.app.admin_asks().lock().unwrap().insert(ask_id, PendingAsk { user, node, at: Instant::now() });
    let ask = AdminAsk { ask_id, role_id: Some(role_id), context, history: hist, message: text.to_string() };
    if !state.app.send_to(node, ServerMsg::AdminAsk(ask)).await {
        state.app.admin_asks().lock().unwrap().remove(&ask_id);
        return Err((StatusCode::CONFLICT, "节点刚刚断线了，没有发出去，请再试一次".into()));
    }
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "ask_id": ask_id, "message_id": message_id }))))
}

/// 不花额度、不动任何东西的忽略。只能是提议的主人，且还是待确认的。
pub(crate) async fn api_dismiss(State(state): State<ApiState>, Path(id): Path<String>, headers: HeaderMap) -> Result<StatusCode, ApiError> {
    let user = user_of(&state, &headers).await?;
    let id: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这条提议".to_string()))?;
    let done = sqlx::query("update admin_proposals set status = 'dismissed', decided_at = now() where id = $1 and user_id = $2 and status = 'pending'")
        .bind(id)
        .bind(user)
        .execute(state.app.pool())
        .await
        .map_err(|e| internal("忽略提议失败", e))?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "没有这条待确认的提议（可能不是你的，或已经处理过）".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// 确认提议：重新核对一遍，然后**用你的身份、在你的节点上**执行。
pub(crate) async fn api_confirm(State(state): State<ApiState>, Path(id): Path<String>, headers: HeaderMap) -> Result<Json<serde_json::Value>, ApiError> {
    let user = user_of(&state, &headers).await?;
    let id: Uuid = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这条提议".to_string()))?;
    let pool = state.app.pool();
    // 先抢占：只有一次点击能把 pending 变成 confirmed，连点两次不会派两遍
    let row = sqlx::query("update admin_proposals set status = 'confirmed', decided_at = now() where id = $1 and user_id = $2 and status = 'pending' returning kind, args, why, node_id")
        .bind(id)
        .bind(user)
        .fetch_optional(pool)
        .await
        .map_err(|e| internal("确认提议失败", e))?
        .ok_or((StatusCode::NOT_FOUND, "没有这条待确认的提议（可能不是你的，或已经处理过）".to_string()))?;
    let proposal = AdminProposal { kind: row.get("kind"), args: row.get("args"), why: row.get("why") };
    let node: Uuid = row.get("node_id");
    let outcome = execute(&state, user, node, &proposal).await;
    let (status, result, extra) = match outcome {
        Ok((result, extra)) => ("confirmed", result, extra),
        Err(reason) => ("failed", reason, serde_json::Value::Null),
    };
    sqlx::query("update admin_proposals set status = $2, result = $3 where id = $1").bind(id).bind(status).bind(result.chars().take(1900).collect::<String>()).execute(pool).await.map_err(|e| internal("记录结果失败", e))?;
    if status == "failed" {
        return Err((StatusCode::CONFLICT, result));
    }
    Ok(Json(serde_json::json!({ "result": result, "next": extra })))
}

async fn execute(state: &ApiState, user: Uuid, node: Uuid, p: &AdminProposal) -> Result<(String, serde_json::Value), String> {
    let pool = state.app.pool();
    // 再核对一遍：这段时间里模块、用例可能变了
    let args = match check_proposal(pool, PROJECT, p).await.map_err(|e| format!("核对提议失败：{e}"))? {
        Checked::Keep(args) => args,
        Checked::Drop(why) => return Err(format!("提议已经过期，没有执行：{why}")),
    };
    match p.kind.as_str() {
        "create_module" => {
            let name = name_arg(&args, "name").unwrap_or_default();
            crate::modules::create_module(pool, PROJECT, &name).await.map_err(|e| e.to_string())?;
            Ok((format!("已新建模块「{name}」"), serde_json::Value::Null))
        }
        "generate_cases" => {
            let module = name_arg(&args, "module").unwrap_or_default();
            // 需要 UI 链接和区域截图，管理员拿不到：这里不执行，告诉前端打开预填好的表单，由人补上再提交
            Ok((format!("请在表单里补上「{module}」的 UI 链接和区域截图后提交"), serde_json::json!({ "open_generate": { "module": module } })))
        }
        "run_tests" => {
            // 节点必须还是这个人的、在线、并且有检测通过的执行员和判定员角色
            match crate::users::check_node_owner(pool, node, user).await.map_err(|e| e.to_string())? {
                crate::users::NodeOwner::Owned => {}
                _ => return Err("这个节点已经不是你的了，不能用它发起测试".into()),
            }
            if !state.app.node_connected(node).await {
                return Err("你的节点不在线，没法发起测试".into());
            }
            let (executor, judge) = default_task_roles(state, node)?;
            let codes: Vec<String> = args.get("case_codes").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let created = runs::create_task(pool, &NewTask { project: PROJECT.into(), node_id: node, case_codes: codes.clone(), executor, judge }).await.map_err(|e| e.to_string())?;
            Ok((format!("已用你的节点发起测试：{} 条用例，任务 {}", created.run_ids.len(), &created.task_id.to_string()[..8]), serde_json::json!({ "task_id": created.task_id })))
        }
        other => Err(format!("不认识的提议类型 {other}")),
    }
}

/// 发起测试用的默认角色：这个节点上第一个检测通过的执行员和判定员，来源和模型都按角色配置（规则 3），不写死。
fn default_task_roles(state: &ApiState, node: Uuid) -> Result<(RoleModel, RoleModel), String> {
    let report = state.app.roles_report(node).ok_or("节点还没有上报角色，请先在「角色与模型」页配置并检测")?;
    let pick = |slot: RoleSlot, label: &str| {
        report
            .roles
            .iter()
            .find(|r| r.config.slot == slot && r.check.status == protocol::CheckStatus::Ok)
            .map(|r| RoleModel { source: r.config.source, model: Some(r.config.default.model.clone()), effort: r.config.default.effort.clone() })
            .ok_or_else(|| format!("节点上没有检测通过的「{label}」角色，请先在「角色与模型」页配置并检测"))
    };
    let executor = pick(RoleSlot::Executor, "执行员")?;
    let judge = pick(RoleSlot::Judge, "判定员")?;
    // 目前节点只能跑这些组合（和创建任务的校验一致）
    if executor.source != ModelSource::CodexExec || !matches!(judge.source, ModelSource::ClaudeCli | ModelSource::Agy) {
        return Err("节点上的执行员或判定员来源暂时不能用来跑测试".into());
    }
    Ok((executor, judge))
}
