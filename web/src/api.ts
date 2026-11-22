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
  recordings: Recording[];
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

interface ModelGroup {
  default: { model: string; effort: string | null };
  models: { model: string; efforts: string[] }[];
}

export interface Models {
  executor: ModelGroup;
  judge: ModelGroup;
}

export interface NewTask {
  project: string;
  node_id: string;
  case_codes: string[];
  executor: { model: string; effort: string };
  judge: { model: string };
}

async function request(path: string, options: RequestInit): Promise<Response> {
  let response: Response;
  try {
    response = await fetch(path, { cache: "no-store", ...options });
  } catch {
    throw new Error("无法连接平台服务端");
  }
  if (!response.ok) {
    const failure = options.method === "POST" ? writeTokenFailure(response.status) : null;
    if (failure) {
      if (failure.clearToken) rememberReviewToken("");
      throw new Error(failure.message);
    }
    throw new Error((await response.text()) || `服务端返回 ${response.status}`);
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

export function evidenceUrl(id: string, name: string): string {
  return `/api/runs/${encodeURIComponent(id)}/files/${encodeURIComponent(name)}`;
}
