// 与平台 API 的公开字段对应。

import { rememberReviewToken, writeTokenFailure } from "./reviewLogic";

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
export type BlockedReason = "setup" | "link" | "node_lost" | "stuck" | "quota" | "other";

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
  queued_at: string;
}

export interface ModuleCase {
  code: string;
  title: string;
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
  slot: "executor" | "judge" | "case_writer" | "planner";
  name: string;
  source: string;
  default: { model: string; effort: string | null };
  /** efforts 为空表示该模型没有单独的推理强度档位（强度已写进模型名） */
  models: { model: string; efforts: string[] }[];
}

export interface Models {
  roles: ModelRole[];
}

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
export async function createTask(task: NewTask, token: string, userName: string): Promise<{ task_id: string; run_ids: string[] }> {
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
/** 「我是谁」放在请求头里。名字不是机密；中文不能直接放进 HTTP 头，所以百分号编码（服务端会还原）。没填就不带。 */
const userHeaders = (userName: string): Record<string, string> => (userName.trim() ? { "x-user-name": encodeURIComponent(userName.trim()) } : {});

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

/** 登记过的名字（节点启动带 NODE_USER_NAME 时登记），给「我是谁」做提示。名字不是机密。 */
/** 创建名字：名字没被占就登记成功；密码服务端只存哈希。不需要复核口令。 */
export async function createUser(name: string, password: string, claimNode?: string): Promise<string> {
  const response = await post("/api/users", { name, password, ...(claimNode ? { claim_node: claimNode } : {}) }, "");
  return ((await response.json()) as { name: string }).name;
}
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
