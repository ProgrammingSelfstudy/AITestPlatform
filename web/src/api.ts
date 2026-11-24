// 与平台 API 的公开字段对应。

import { rememberReviewToken, writeTokenFailure } from "./reviewLogic";
import { cleanName, forgetUserToken, readUserToken, rememberUserToken } from "./identity";

export type DeviceStatus = "idle" | "busy" | "offline";

export interface Device {
  serial: string;
  platform: string;
  model: string | null;
  os_version: string | null;
  screen_w: number | null;
  screen_h: number | null;
  status: DeviceStatus;
}

export interface ModelSourceCap {
  source: "claude_cli" | "codex_exec";
  vision: boolean;
}

export interface NodeView {
  node_id: string;
  online: boolean;
  caps: { node_version: string; os: string; model_sources: ModelSourceCap[] };
  devices: Device[];
  last_seen: string;
  /** 节点主人的名字（启动节点时的 NODE_USER_NAME）；没有属主为空。用来显示「这是谁的节点」 */
  owner?: string | null;
}

export type RunStatus = "queued" | "assigned" | "running" | "judging" | "finished" | "blocked" | "cancelled" | "node_lost";
export type Verdict = "passed" | "failed" | "uncertain";
export type FinalResult = Verdict | "blocked";
export type JudgeStatus = "completed" | "failed" | "not_run";
export type BlockedReason = "setup" | "link" | "node_lost" | "stuck" | "quota" | "other" | "skipped";

interface RoleModel {
  source: ModelSourceCap["source"];
  model: string | null;
  effort: string | null;
}

interface TaskInfo {
  task_id: string;
  project: string;
  node_id: string;
  created_at: string;
  executor: RoleModel;
  judge: RoleModel;
}

export interface TaskSummary extends TaskInfo {
  run_count: number;
  status_counts: Record<RunStatus, number>;
  result_summary: string;
}

export interface RunView {
  run_id: string;
  code: string;
  case_version: number;
  status: RunStatus;
  executor_result: Verdict | null;
  judge_status: JudgeStatus | null;
  final_result: FinalResult | null;
  blocked_reason: BlockedReason | null;
  queued_at: string;
  started_at: string | null;
  finished_at: string | null;
  device_serial: string | null;
}

export interface TaskDetail extends TaskInfo {
  runs: RunView[];
}

interface Expected {
  idx: number;
  text: string;
}

export interface RunStep {
  idx: number;
  at_ms?: number | null;
  tool: string;
  args: { target?: string | null; point?: [number, number] | null; value?: unknown } | null;
  result: { new_labels?: string[] | null } | null;
  file_ref: string | null;
  ts: string;
}

export interface JudgeItem {
  idx: number;
  expected_text: string;
  verdict: Verdict;
  evidence: string;
  file_refs: string[];
}

/** 多个判定员同时评审同一份证据时，每个判定员各自的结果（单判定员的 run 没有） */
export interface JudgeOpinion {
  /** 0 是主判定员，1 起是其他判定员 */
  seq: number;
  source: string;
  model: string | null;
  effort: string | null;
  judge_status: "completed" | "failed" | "not_run";
  final_result: "passed" | "failed" | "uncertain";
  items: JudgeItem[];
}

export interface Recording {
  seq: number;
  url: string;
  size: number;
  start_epoch_ms: number | null;
  duration_ms: number | null;
}

export interface RunDetail extends RunView {
  task_id: string;
  case_spec: { code: string; title: string; steps: string[]; expected: Expected[] };
  executor_reason: string | null;
  cost: { model?: string | null; effort?: string | null; wall_s?: number; tool_calls?: number } | null;
  run_steps: RunStep[];
  judge_items: JudgeItem[];
  /** 多个判定员时每个人各自的结果；单判定员为空数组 */
  judge_opinions?: JudgeOpinion[];
  files: string[];
  file_urls: Record<string, string>;
  recordings: Recording[];
  evidence_expires_at: number;
  evidence_key_id: string;
}

export type ReviewMark = "correct" | "passed" | "failed";

export interface ReviewSubmission {
  item_idx: number | null;
  mark: ReviewMark;
  note: string | null;
  reviewer: string;
}

export interface ReviewRecord extends ReviewSubmission {
  id: string;
  run_id: string;
  judge_verdict: Verdict;
  created_at: string;
}

export interface RunReviews {
  history: ReviewRecord[];
  current: ReviewRecord[];
}

export interface ReviewStats {
  reviewed: number;
  judge_correct_rate: number | null;
}

export interface CaseSummary {
  code: string;
  title: string;
  should_fail: boolean;
  status: string;
  blocked_reason: string | null;
  /** 生成的用例要先复核和人工确认（approved）才能执行；导入的老用例恒为 approved */
  review_status: ReviewStatus;
}

/** 生成用例的复核状态：评审员的「通过」（reviewed_ok）只是意见，必须人确认（approved）才能执行 */
export type ReviewStatus = "draft" | "reviewed_ok" | "needs_human" | "approved" | "rejected";

export type CasegenStatus = "queued" | "running" | "finished" | "failed" | "cancelled";

export interface CasegenRequestView {
  id: string;
  status: CasegenStatus;
  /** 失败原因或成功说明，原样给用户看 */
  detail: string;
  requested_at: string;
}

export interface ModuleView {
  id: string;
  name: string;
  created_at: string;
  case_count: number;
  approved_count: number;
  needs_human_count: number;
  /** 排队、分配、执行、判定中的 run 数；大于 0 就是「测试中」，此时才用指挥台布局 */
  active_runs: number;
  latest_request: CasegenRequestView | null;
}

export interface LatestRun {
  run_id: string;
  task_id: string;
  status: string;
  final_result: "passed" | "failed" | "uncertain" | "blocked" | null;
  blocked_reason: string | null;
  /** 条件不足被跳过时的具体原因（blocked_reason 是 skipped 才有） */
  skip_reason: string | null;
  queued_at: string;
}

export interface ModuleCase {
  code: string;
  title: string;
  /** 运行环境：app / h5 / browser（和 CSV 里的「运行环境」一致） */
  run_env: "app" | "h5" | "browser";
  level: string;
  preconditions: string[];
  origin: "imported" | "generated";
  review_status: ReviewStatus;
  /** 评审员的意见和人的备注，原样显示 */
  review_note: string;
  steps: string[];
  expected: string[];
  latest_run: LatestRun | null;
}

export interface CasegenRequestDetail {
  id: string;
  module: string;
  status: CasegenStatus;
  detail: string;
  ui_links: string[];
  screenshot_count: number;
  doc_sha256: string;
  requested_at: string;
}

/** 创建任务的必填项：模块、需求文档、UI 图链接、UI 区域截图（base64，AI 只看截图，不会打开链接） */
export interface CasegenInput {
  module: string;
  doc: string;
  ui_links: string[];
  screenshots: { data_b64: string }[];
  node_id: string;
  writer_role?: string;
  reviewer_role?: string;
}

/** 一个角色 = 某个岗位（执行员 / 判定员）可选的一个来源和它的模型。新增角色或模型只改服务端配置。 */
export interface ModelRole {
  id: string;
  slot: "executor" | "judge" | "case_writer" | "planner" | "ui_review";
  name: string;
  source: string;
  default: { model: string; effort: string | null };
  /** efforts 为空表示该模型没有单独的推理强度档位（强度已写进模型名） */
  models: { model: string; efforts: string[] }[];
}

export interface Models {
  roles: ModelRole[];
}

/** 每个岗位最多几个角色（和 crates/protocol 的 RoleSlot::MAX_PER_SLOT 保持一致，服务端保存时也会校验） */
export const ROLE_SLOT_LIMIT = 3;

/** 模型来源。除 deepseek 外都是本机会员登录，只有 deepseek 用 API Key（规则 1）。 */
export const ROLE_SOURCES: { value: string; label: string; needsKey: boolean; available: boolean; hint: string }[] = [
  { value: "codex_exec", label: "Codex（本机 ChatGPT 登录）", needsKey: false, available: true, hint: "只能当执行员" },
  { value: "claude_cli", label: "Claude（本机会员登录）", needsKey: false, available: true, hint: "只能当判定员" },
  { value: "agy", label: "Gemini（agy，个人账号登录）", needsKey: false, available: true, hint: "只能当判定员；模型名已带推理强度" },
  { value: "deepseek", label: "DeepSeek（API Key）", needsKey: true, available: false, hint: "运行器还在开发中，暂时不能配置" },
];

export type CheckStatus = "unchecked" | "checking" | "ok" | "not_logged_in" | "quota_exhausted" | "failed";

export interface RoleCheck {
  status: CheckStatus;
  /** 失败时的原始错误（已脱敏，不含 Key），要原样给用户看 */
  detail: string | null;
  checked_at: string | null;
}

/** 节点上报的一个角色：配置 + 是否已配 Key（只有布尔，永远不回显 Key）+ 最近一次检测 */
export interface RoleView extends ModelRole {
  key_configured: boolean;
  check: RoleCheck;
}

export interface NodeRoles {
  /** 节点离线时拿不到角色，页面要明确提示 */
  online: boolean;
  roles: RoleView[];
  /** 这个节点是不是你的：没填名字为 null（只读），填了名字是 true / false。数据大家都能看，只有主人能改、能检测（花他的额度） */
  mine: boolean | null;
  /** 名字没登记等原因，原样显示 */
  name_problem: string | null;
  /** 节点还没有主人，当前名字可以认领 */
  unowned?: boolean;
}

/** 保存时提交的角色。api_key 只在需要新设或更换 Key 时填；不填表示保持原来的。 */
export interface RoleInput extends ModelRole {
  api_key?: string;
}

export type ScriptStatus = "candidate" | "self_check_passed" | "approved" | "rejected" | "retired";

export interface Regression {
  id: string;
  passed: boolean;
  serial: string;
  detail: string;
  duration_ms: number;
  created_at: string;
}

/** 自愈分诊：locator_broken 定位失效（已产出修复候选）、business_bug 业务行为错误（保留失败）、unsure 分不清（交人工） */
export interface Triage {
  regression_id: string;
  verdict: "locator_broken" | "business_bug" | "unsure";
  reason: string;
  healed_script_id: string | null;
  healed_version: number | null;
  created_at: string;
}

/** 页面发起的回归请求：排队 → 节点领取运行 → 完成（得到结论）或失败（没得到结论）。结论本身在 last_regression。 */
export interface RegressionRequest {
  id: string;
  node_id: string;
  status: "queued" | "running" | "finished" | "failed" | "cancelled";
  detail: string;
  requested_at: string;
}

/** 回归脚本的一个版本（只有元数据，不含源码） */
export interface ScriptRow {
  id: string;
  case_code: string;
  case_title: string;
  version: number;
  case_version: number;
  source_run_id: string;
  file_names: string[];
  gen_source: "claude_cli" | "codex_exec";
  gen_model: string | null;
  gen_effort: string | null;
  notes: string;
  status: ScriptStatus;
  status_note: string | null;
  last_regression: Regression | null;
  last_request: RegressionRequest | null;
  last_triage: Triage | null;
  /** 自愈产出的候选：它是从哪个版本修来的 */
  healed_from: string | null;
  created_at: string;
  updated_at: string;
}

export interface NewTask {
  project: string;
  node_id: string;
  case_codes: string[];
  executor: { role: string; model: string; effort?: string };
  judge: { role: string; model: string };
  /** 其他判定员：和 judge 同时评审同一份证据、各自出结论；不能和 judge 或彼此重复 */
  more_judges?: { role: string; model: string }[];
}

async function request(path: string, options: RequestInit): Promise<Response> {
  let response: Response;
  try {
    response = await fetch(path, { cache: "no-store", ...options });
  } catch {
    throw new Error("无法连接平台服务端");
  }
  if (!response.ok) {
    const body = await response.text().catch(() => "");
    // 口令校验失败（403/429）服务端不带正文；带正文的 403 是别的原因（例如不是自己的节点），
    // 要显示那个原因，不能冒充成口令问题，更不能因此清掉用户输对的口令
    const failure = options.method === "POST" && !body ? writeTokenFailure(response.status) : null;
    if (failure) {
      if (failure.clearToken) rememberReviewToken("");
      throw new Error(failure.message);
    }
    throw new Error(body || `服务端返回 ${response.status}`);
  }
  return response;
}

async function get<T>(path: string, signal?: AbortSignal): Promise<T> {
  const response = await request(path, { signal });
  try {
    return (await response.json()) as T;
  } catch {
    throw new Error("服务端返回的数据格式无效");
  }
}

function post(path: string, body: object, token: string, extraHeaders: Record<string, string> = {}) {
  return request(path, {
    method: "POST",
    redirect: "error",
    headers: { "Content-Type": "application/json", "X-Requested-With": "ai-test-platform", "x-review-token": token, ...extraHeaders },
    body: JSON.stringify(body),
  });
}

export const fetchNodes = (signal?: AbortSignal) => get<NodeView[]>("/api/nodes", signal);
export const fetchTasks = (signal?: AbortSignal) => get<TaskSummary[]>("/api/tasks", signal);
export const fetchTask = (id: string, signal?: AbortSignal) => get<TaskDetail>(`/api/tasks/${encodeURIComponent(id)}`, signal);
export const fetchRun = (id: string, signal?: AbortSignal) => get<RunDetail>(`/api/runs/${encodeURIComponent(id)}`, signal);
const runReviewsPath = (id: string) => `/api/runs/${encodeURIComponent(id)}/reviews`;
export const fetchRunReviews = (id: string, signal?: AbortSignal) => get<RunReviews>(runReviewsPath(id), signal);
export const fetchReviewStats = (signal?: AbortSignal) => get<ReviewStats>("/api/reviews/stats", signal);
export const fetchModels = (signal?: AbortSignal) => get<Models>("/api/models", signal);
export const fetchScripts = (project: string, signal?: AbortSignal) =>
  get<ScriptRow[]>(`/api/projects/${encodeURIComponent(project)}/scripts`, signal);
export const fetchCases = (project: string, signal?: AbortSignal) =>
  get<CaseSummary[]>(`/api/projects/${encodeURIComponent(project)}/cases`, signal);

/** `userName` 是发起人的名字（只记名字、靠自觉）。服务端据此确认所选节点是他的：数据大家都能跑，但只能用自己的节点、花自己的额度。 */
export async function createTask(task: NewTask, token: string, userName: string): Promise<{ task_id: string; run_ids: string[]; skipped: { code: string; reason: string }[] }> {
  const response = await post("/api/tasks", task, token, userHeaders(userName));
  return response.json();
}

export async function cancelRun(id: string, token: string): Promise<void> {
  await post(`/api/runs/${encodeURIComponent(id)}/cancel`, {}, token);
}

/** 获准、下线、拒绝脚本：人的决定，用审核口令。原因下线和拒绝必填，获准可不写。 */
export async function decideScript(id: string, action: "approve" | "retire" | "reject", note: string, token: string): Promise<void> {
  await post(`/api/scripts/${encodeURIComponent(id)}/${action}`, { note }, token);
}

/** 在自己的节点上排一次回归：审核口令加个人令牌（和发起任务同一套），个人令牌只在本次请求中使用。 */
export async function requestRegression(scriptId: string, nodeId: string, token: string, userName: string): Promise<void> {
  await post(`/api/scripts/${encodeURIComponent(scriptId)}/regress`, { node_id: nodeId }, token, userHeaders(userName));
}

/** 取消还在排队的回归请求；节点已经领走的取消不了。 */
export async function cancelRegression(requestId: string, token: string, userName: string): Promise<void> {
  await post(`/api/regression-requests/${encodeURIComponent(requestId)}/cancel`, {}, token, userHeaders(userName));
}

export async function submitReview(id: string, review: ReviewSubmission, token: string): Promise<number> {
  try {
    const response = await fetch(runReviewsPath(id), {
      method: "POST",
      cache: "no-store",
      redirect: "error",
      headers: {
        "Content-Type": "application/json",
        "X-Requested-With": "ai-test-platform",
        "x-review-token": token,
      },
      body: JSON.stringify(review),
    });
    return response.status;
  } catch {
    throw new Error("无法提交审核，请检查连接后重试");
  }
}

export function evidenceUrl(run: RunDetail, name: string): string | undefined {
  return run.file_urls[name];
}

// ---------------------------------------------------------------- 角色与模型配置（每个节点各自一份，只能操作自己的节点）

const rolesPath = (nodeId: string) => `/api/nodes/${encodeURIComponent(nodeId)}/roles`;
/** 「我是谁」用百分号编码传名字。已登录时一并带上该名字的会话令牌。 */
function userHeaders(userName: string): Record<string, string> {
  const name = cleanName(userName);
  if (!name) return {};
  const headers: Record<string, string> = { "x-user-name": encodeURIComponent(name) };
  const token = readUserToken(name);
  if (token) headers["x-user-token"] = token;
  return headers;
}

export async function fetchNodeRoles(nodeId: string, userName: string, signal?: AbortSignal): Promise<NodeRoles> {
  const response = await request(rolesPath(nodeId), { signal, headers: userHeaders(userName) });
  try {
    return (await response.json()) as NodeRoles;
  } catch {
    throw new Error("服务端返回的数据格式无效");
  }
}

/** 整体替换该节点的角色配置，服务端转给节点写入本机；Key 只经这一次转手，不落库。 */
export async function saveNodeRoles(nodeId: string, roles: RoleInput[], token: string, userName: string): Promise<void> {
  await post(rolesPath(nodeId), { roles }, token, userHeaders(userName));
}

/** 解除节点和主人的绑定。只有主人能解除。解除后别人可以认领；节点若仍带原来的 NODE_USER_NAME 重启会再绑回去。 */
export async function releaseNode(nodeId: string, token: string, userName: string): Promise<void> {
  await post(`/api/nodes/${encodeURIComponent(nodeId)}/release`, {}, token, userHeaders(userName));
}

/** 认领一个没有主人的节点（要先说明你是谁；有密码的名字要先登录）。已经有主人不能抢。 */
export async function claimNode(nodeId: string, userName: string): Promise<void> {
  await post(`/api/nodes/${encodeURIComponent(nodeId)}/claim`, {}, "", userHeaders(userName));
}

/** 管理员强制解除：用管理员账号（名字 + 密码）验证，不要求是节点主人，也不需要复核口令。 */
export async function forceReleaseNode(nodeId: string, name: string, password: string): Promise<void> {
  await post(`/api/nodes/${encodeURIComponent(nodeId)}/force-release`, { name, password }, "");
}

/** 节点还没有任何角色时，用服务端模板（config/models.json）初始化，不带 Key。 */
export async function initNodeRoles(nodeId: string, token: string, userName: string): Promise<void> {
  await post(`${rolesPath(nodeId)}/init-template`, {}, token, userHeaders(userName));
}

/** 让节点真调一次模型，检测这个角色能不能用；结果通过 fetchNodeRoles 轮询得到。 */
export async function checkNodeRole(nodeId: string, roleId: string, token: string, userName: string): Promise<void> {
  await post(`${rolesPath(nodeId)}/${encodeURIComponent(roleId)}/check`, {}, token, userHeaders(userName));
}

// ---------------------------------------------------------------- 模块、用例生成与复核

export const fetchModules = (project: string, signal?: AbortSignal) =>
  get<ModuleView[]>(`/api/projects/${encodeURIComponent(project)}/modules`, signal);
export const fetchModuleCases = (moduleId: string, signal?: AbortSignal) =>
  get<ModuleCase[]>(`/api/modules/${encodeURIComponent(moduleId)}/cases`, signal);
export const fetchCasegenRequest = (id: string, signal?: AbortSignal) =>
  get<CasegenRequestDetail>(`/api/casegen-requests/${encodeURIComponent(id)}`, signal);

/** 创建任务（生成用例）：模块、需求文档、UI 链接、区域截图都必填，缺哪项服务端说哪项；只能发到自己的节点 */
export async function createCasegenRequest(project: string, input: CasegenInput, token: string, userName: string): Promise<{ request_id: string; module_id: string; doc_id: string }> {
  const response = await post(`/api/projects/${encodeURIComponent(project)}/casegen-requests`, input, token, userHeaders(userName));
  return response.json();
}

/** 人的决定：批准（才能执行）或拒绝。评审员有疑问（needs_human）的用例，批准时必须写备注。只有发起这个模块生成请求的人能操作。 */
export async function decideCase(moduleId: string, code: string, decision: "approve" | "reject", note: string, token: string, userName: string): Promise<void> {
  await post(`/api/modules/${encodeURIComponent(moduleId)}/cases/${encodeURIComponent(code)}/decision`, { decision, note }, token, userHeaders(userName));
}

/** 一键批准评审员认为没问题的（reviewed_ok）；needs_human 的不在内，必须逐条处理。返回批准了几条。 */
export async function approveReviewed(moduleId: string, token: string, userName: string): Promise<number> {
  const response = await post(`/api/modules/${encodeURIComponent(moduleId)}/approve-reviewed`, {}, token, userHeaders(userName));
  return ((await response.json()) as { approved: number }).approved;
}

// ---------------------------------------------------------------- 选应用 → 检查本机环境 → 取模型

export type AppLogin = "ok" | "not_logged_in" | "wrong_auth" | "not_applicable";

/** 应用里的一个模型。efforts 为空表示没有单独的推理强度档位（强度已写进模型名，如 agy）。 */
export interface AppModel {
  id: string;
  display: string;
  efforts: string[];
  default_effort: string | null;
}

/** 四个应用各一项。scanned=false 表示还没检查过，其余字段不存在。 */
export interface AppView {
  /** claude_cli / codex_exec / agy / deepseek */
  source: string;
  scanned: boolean;
  installed?: boolean;
  version?: string | null;
  login?: AppLogin;
  /** 登录检查的说明（不含账号信息），原样显示 */
  login_detail?: string;
  models?: AppModel[];
  /** 模型列表的来源说明（例如 Claude 命令行不能列模型，这里是别名建议、可手填），原样显示 */
  models_note?: string;
  scanned_at?: string;
}

export interface NodeApps {
  /** 节点离线时拿不到检查结果 */
  online: boolean;
  apps: AppView[];
  mine: boolean | null;
  name_problem: string | null;
  unowned?: boolean;
}

export async function fetchNodeApps(nodeId: string, userName: string, signal?: AbortSignal): Promise<NodeApps> {
  const response = await request(`/api/nodes/${encodeURIComponent(nodeId)}/apps`, { signal, headers: userHeaders(userName) });
  try {
    return (await response.json()) as NodeApps;
  } catch {
    throw new Error("服务端返回的数据格式无效");
  }
}

/** 让节点检查本机有没有这个应用、怎么登录的、有哪些模型。只跑本地命令，不调模型、不花额度，不需要审核口令。结果靠 fetchNodeApps 轮询得到。 */
export async function scanNodeApp(nodeId: string, source: string, userName: string): Promise<void> {
  await post(`/api/nodes/${encodeURIComponent(nodeId)}/apps/${encodeURIComponent(source)}/scan`, {}, "", userHeaders(userName));
}

// ---------------------------------------------------------------- 谁在用

/** 创建名字：名字没被占就登记成功；密码服务端只存哈希。不需要复核口令。 */
export async function createUser(name: string, password: string, claimNode?: string): Promise<string> {
  const response = await post("/api/users", { name, password, ...(claimNode ? { claim_node: claimNode } : {}) }, "");
  const result = await response.json().catch(() => null) as { name?: unknown } | null;
  if (typeof result?.name !== "string" || !result.name) throw new Error("创建名字的响应格式无效");
  return result.name;
}

export async function loginUser(name: string, password: string): Promise<string> {
  const response = await post("/api/login", { name, password }, "");
  const result = await response.json().catch(() => null) as { name?: unknown; token?: unknown } | null;
  if (result?.name !== cleanName(name) || typeof result.token !== "string" || !result.token) throw new Error("登录响应格式无效");
  rememberUserToken(result.name, result.token);
  return result.name;
}

export async function logoutUser(name: string): Promise<void> {
  const headers = userHeaders(name);
  forgetUserToken(name);
  if (headers["x-user-token"]) await post("/api/logout", {}, "", headers);
}

/** 登记过的名字，给「我是谁」做提示。 */
export const fetchUsers = (signal?: AbortSignal) => get<string[]>("/api/users", signal);

// ---------------------------------------------------------------- 管理员：只提议，你点确认才执行

export type ProposalStatus = "pending" | "confirmed" | "dismissed" | "failed";

/** 管理员的一条提议。kind 只有三种：create_module 新建模块 / generate_cases 生成用例（要你补 UI 链接和截图）/ run_tests 对已批准的用例发起测试。 */
export interface AdminProposal {
  id: string;
  kind: "create_module" | "generate_cases" | "run_tests";
  args: { name?: string; module?: string; case_codes?: string[] };
  why: string;
  status: ProposalStatus;
  /** 执行结果或失败原因，原样显示 */
  result: string;
}

export interface AdminMessage {
  id: string;
  author: "user" | "admin";
  text: string;
  /** 管理员没回答成功（没有角色、额度用完、超时…），text 是原因 */
  is_error: boolean;
  created_at: string;
  proposals: AdminProposal[];
}

export interface AdminChat {
  /** 正在等管理员回答 */
  pending: { ask_id: string; since: string } | null;
  messages: AdminMessage[];
}

export const fetchAdminChat = (userName: string, signal?: AbortSignal) =>
  request("/api/admin/chat", { signal, headers: userHeaders(userName) }).then((r) => r.json() as Promise<AdminChat>);

/** 对管理员说一句话。要审核口令；回答由你自己节点上的管理员角色生成，用的是你自己的额度。 */
export async function sendAdminMessage(message: string, token: string, userName: string): Promise<void> {
  await post("/api/admin/messages", { message }, token, userHeaders(userName));
}

/** 确认一条提议：服务端重新核对后用你的身份、在你的节点上执行。返回结果说明和下一步（打开预填的表单、或任务编号）。 */
export async function confirmProposal(id: string, token: string, userName: string): Promise<{ result: string; next: { open_generate?: { module: string }; task_id?: string } | null }> {
  const response = await post(`/api/admin/proposals/${encodeURIComponent(id)}/confirm`, {}, token, userHeaders(userName));
  return response.json();
}

/** 忽略一条提议，不执行任何东西。 */
export async function dismissProposal(id: string, userName: string): Promise<void> {
  await post(`/api/admin/proposals/${encodeURIComponent(id)}/dismiss`, {}, "", userHeaders(userName));
}

// ---------------------------------------------------------------- 参数配置（P1 只读）

export type ConfigStatus = "normal" | "disabled";

export interface ConfigType {
  id: number;
  name: string;
  code: string;
  remark: string;
  status: ConfigStatus;
  item_count: number;
  updated_at: string;
  updated_by: string | null;
}

export interface ConfigItem {
  id: number;
  name: string;
  code: string;
  value: string;
  label: string;
  group_name: string;
  sort: number;
  remark: string;
  status: ConfigStatus;
  updated_at: string;
  updated_by: string | null;
}

export interface Paged<T> {
  total: number;
  page: number;
  page_size: number;
  rows: T[];
  /** 当前这个人能不能改配置（只有登录的管理员） */
  can_write: boolean;
}

export interface ConfigTypeDetail {
  type: ConfigType;
  items: Paged<ConfigItem>;
  can_write: boolean;
}

export interface ConfigTypeInput {
  name: string;
  code?: string;
  remark: string;
  status: ConfigStatus;
}

export interface ConfigItemInput {
  name: string;
  code?: string;
  value: string;
  label: string;
  group_name: string;
  sort: number;
  remark: string;
  status: ConfigStatus;
}

export const fetchConfigTypes = (query: { name?: string; code?: string; page: number; pageSize: number }, userName: string, signal?: AbortSignal) => {
  const params = new URLSearchParams({ page: String(query.page), page_size: String(query.pageSize) });
  if (query.name?.trim()) params.set("name", query.name.trim());
  if (query.code?.trim()) params.set("code", query.code.trim());
  return getWith<Paged<ConfigType>>(`/api/configs?${params}`, userName, signal);
};

export const fetchConfigDetail = (code: string, page: number, pageSize: number, userName: string, signal?: AbortSignal) =>
  getWith<ConfigTypeDetail>(`/api/configs/${encodeURIComponent(code)}?page=${page}&page_size=${pageSize}`, userName, signal);

/** 带「我是谁」和会话令牌的读取：服务端据此告诉页面能不能写 */
async function getWith<T>(path: string, userName: string, signal?: AbortSignal): Promise<T> {
  const response = await request(path, { signal, headers: userHeaders(userName) });
  try {
    return (await response.json()) as T;
  } catch {
    throw new Error("服务端返回的数据格式无效");
  }
}

// 写入只给管理员：身份靠名字 + 登录会话，不需要审核口令。全部用 POST（浏览器写入守卫只管 POST）。
const configPath = (code: string) => `/api/configs/${encodeURIComponent(code)}`;
const adminPost = (path: string, body: object, userName: string) => post(path, body, "", userHeaders(userName));

export const createConfigType = async (input: ConfigTypeInput, userName: string) => { await adminPost("/api/configs", input, userName); };
export const updateConfigType = async (code: string, input: ConfigTypeInput, userName: string) => { await adminPost(`${configPath(code)}/update`, input, userName); };
export const deleteConfigType = async (code: string, userName: string) => { await adminPost(`${configPath(code)}/delete`, {}, userName); };
export const createConfigItem = async (code: string, input: ConfigItemInput, userName: string) => { await adminPost(`${configPath(code)}/items`, input, userName); };
export const updateConfigItem = async (code: string, id: number, input: ConfigItemInput, userName: string) => { await adminPost(`${configPath(code)}/items/${id}/update`, input, userName); };
export const deleteConfigItem = async (code: string, id: number, userName: string) => { await adminPost(`${configPath(code)}/items/${id}/delete`, {}, userName); };

/** 每个岗位最多几个角色：读参数配置里的 role_limit，读不到或值不合法就用默认 ROLE_SLOT_LIMIT（服务端保存时也是这个规则） */
export async function fetchRoleLimits(signal?: AbortSignal): Promise<Record<string, number>> {
  const limits: Record<string, number> = {};
  try {
    const detail = await get<ConfigTypeDetail>("/api/configs/role_limit?page=1&page_size=100", signal);
    if (detail.type.status !== "normal") return limits;
    for (const item of detail.items.rows) {
      const value = Number(item.value.trim());
      if (item.status === "normal" && Number.isInteger(value) && value >= 1 && value <= 20) limits[item.code] = value;
    }
  } catch {
    /* 读不到就全部用默认值 */
  }
  return limits;
}

export interface ConfigDocument {
  types: { name: string; code: string; remark?: string; status?: ConfigStatus; items?: { name: string; code: string; value: string; label?: string; group_name?: string; sort?: number; remark?: string; status?: ConfigStatus }[] }[];
}

export interface ImportResult {
  created_types: number;
  updated_types: number;
  skipped_types: number;
  created_items: number;
  updated_items: number;
  skipped_items: number;
}

/** 导出全部配置（也是导入文件的模板） */
export const fetchConfigExport = (signal?: AbortSignal) => get<ConfigDocument>("/api/configs/export", signal);

/** 导入：先整体校验再写入；`skip` 已有的编码跳过，`overwrite` 用文件内容覆盖。只给管理员。 */
export async function importConfigs(document: ConfigDocument, mode: "skip" | "overwrite", userName: string): Promise<ImportResult> {
  const response = await adminPost("/api/configs/import", { mode, document }, userName);
  return response.json();
}

/** 流程模板：参数配置里 `flow_template` 类型下的配置项，值是 JSON，岗位对应角色名称 */
export interface FlowTemplate {
  code: string;
  name: string;
  remark: string;
  /** 岗位（executor / judge / case_writer / planner / ui_review）→ 角色名称列表（一个岗位可以配多个，按顺序优先） */
  roles: Record<string, string[]>;
}

/** 岗位的中文名和显示顺序 */
export const SLOT_LABEL: Record<ModelRole["slot"], string> = {
  executor: "执行员",
  judge: "判定员",
  case_writer: "测试用例",
  planner: "管理员",
  ui_review: "UI 走查",
};
export const SLOT_ORDER: ModelRole["slot"][] = ["executor", "judge", "case_writer", "planner", "ui_review"];

/** 解析流程模板的值。每个岗位可以是一个名字（旧格式）或名字列表，统一成列表；格式不对返回 null */
export function parseFlowRoles(value: string): Record<string, string[]> | null {
  try {
    const parsed = JSON.parse(value) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return null;
    const roles: Record<string, string[]> = {};
    for (const [slot, names] of Object.entries(parsed)) {
      const list = (Array.isArray(names) ? names : [names]).filter((n): n is string => typeof n === "string" && n.trim() !== "").map((n) => n.trim());
      if (list.length > 0) roles[slot] = [...new Set(list)];
    }
    return Object.keys(roles).length > 0 ? roles : null;
  } catch {
    return null;
  }
}

/** 把流程里的角色按岗位顺序写成「执行员：A；判定员：B、C」，给表格显示用 */
export function describeFlowRoles(roles: Record<string, string[]>): string {
  return SLOT_ORDER.filter((slot) => roles[slot]?.length).map((slot) => `${SLOT_LABEL[slot]}：${roles[slot].join("、")}`).join("；");
}

/** 读所有可用的流程模板（类型和配置项都是「正常」、值格式对的）；读不到就当没有，不挡住手动选角色 */
export async function fetchFlowTemplates(signal?: AbortSignal): Promise<FlowTemplate[]> {
  try {
    const detail = await get<ConfigTypeDetail>("/api/configs/flow_template?page=1&page_size=100", signal);
    if (detail.type.status !== "normal") return [];
    return detail.items.rows.flatMap((item) => {
      const roles = item.status === "normal" ? parseFlowRoles(item.value) : null;
      return roles ? [{ code: item.code, name: item.name, remark: item.remark, roles }] : [];
    });
  } catch {
    return [];
  }
}

// ---------------------------------------------------------------- 模块详情：UI 图、用例 CSV、录屏

export interface ModuleShot {
  id: string;
  name: string;
  content_type: string;
  size: number;
  created_at: string;
  created_by: string | null;
}

export interface CaseRecordings {
  code: string;
  title: string;
  run_id: string;
  final_result: LatestRun["final_result"];
  recordings: { seq: number; url: string; duration_ms: number | null; start_epoch_ms: number | null }[];
  expires_at: number;
}

const moduleBase = (moduleId: string) => `/api/modules/${encodeURIComponent(moduleId)}`;

export const fetchModuleShots = (moduleId: string, signal?: AbortSignal) => get<ModuleShot[]>(`${moduleBase(moduleId)}/screenshots`, signal);
export const moduleShotUrl = (moduleId: string, shotId: string) => `${moduleBase(moduleId)}/screenshots/${encodeURIComponent(shotId)}`;
export const fetchModuleRecordings = (moduleId: string, signal?: AbortSignal) => get<CaseRecordings[]>(`${moduleBase(moduleId)}/recordings`, signal);
export const moduleCasesCsvUrl = (moduleId: string) => `${moduleBase(moduleId)}/cases.csv`;

/** 给模块加 UI 图：一次最多 100 张，单张 ≤ 3MB（PNG / JPEG），模块里合计最多 100 张、100MB；同一张图不重复存 */
export async function addModuleShots(moduleId: string, shots: { name: string; data_b64: string }[], token: string, userName: string): Promise<{ added: number; duplicates: number }> {
  const response = await post(`${moduleBase(moduleId)}/screenshots`, { screenshots: shots }, token, userHeaders(userName));
  return response.json();
}

export async function deleteModuleShot(moduleId: string, shotId: string, token: string, userName: string): Promise<void> {
  await post(`${moduleBase(moduleId)}/screenshots/${encodeURIComponent(shotId)}/delete`, {}, token, userHeaders(userName));
}

/** 导入用例 CSV：整体校验，有问题整个文件都不写，错误里写明第几行 */
export async function importModuleCases(moduleId: string, csv: string, token: string, userName: string): Promise<{ inserted: number; updated: number; unchanged: number }> {
  const response = await post(`${moduleBase(moduleId)}/cases/import`, { csv }, token, userHeaders(userName));
  return response.json();
}

/** 读文本（导出 CSV 用）：带「我是谁」，错误原样抛出 */
export async function fetchText(path: string, signal?: AbortSignal): Promise<string> {
  const response = await request(path, { signal });
  return response.text();
}
