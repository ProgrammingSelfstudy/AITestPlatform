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
}

/** 一个角色 = 某个岗位（执行员 / 判定员）可选的一个来源和它的模型。新增角色或模型只改服务端配置。 */
export interface ModelRole {
  id: string;
  slot: "executor" | "judge";
  name: string;
  source: string;
  default: { model: string; effort: string | null };
  /** efforts 为空表示该模型没有单独的推理强度档位（强度已写进模型名） */
  models: { model: string; efforts: string[] }[];
}

export interface Models {
  roles: ModelRole[];
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

/** `userToken` 是发起人的个人令牌，只在内存里用一次，不落本地存储。服务端据此确认节点属于发起人。 */
export async function createTask(task: NewTask, token: string, userToken: string): Promise<{ task_id: string; run_ids: string[] }> {
  const response = await post("/api/tasks", task, token, { "x-user-token": userToken });
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
export async function requestRegression(scriptId: string, nodeId: string, token: string, userToken: string): Promise<void> {
  await post(`/api/scripts/${encodeURIComponent(scriptId)}/regress`, { node_id: nodeId }, token, { "x-user-token": userToken });
}

/** 取消还在排队的回归请求；节点已经领走的取消不了。 */
export async function cancelRegression(requestId: string, token: string, userToken: string): Promise<void> {
  await post(`/api/regression-requests/${encodeURIComponent(requestId)}/cancel`, {}, token, { "x-user-token": userToken });
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
