import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import NewTaskDialog from "../src/NewTaskDialog";
import { retryableRunCodes } from "../src/TasksPage";
import { rememberUserName } from "../src/identity";
import { rememberReviewToken } from "../src/reviewLogic";

const fetchMock = vi.fn<typeof fetch>();
const NODE = "aaaaaaaa-0000-0000-0000-000000000001";

const nodeRole = (id: string, slot: string, name: string, status: "ok" | "unchecked" = "ok", model = "m") => ({
  id, slot, name, source: slot === "executor" ? "codex_exec" : "claude_cli", default: { model, effort: null }, models: [{ model, efforts: [] }], key_configured: false,
  check: { status, detail: null, checked_at: null },
});
const configRole = (id: string, slot: string, name: string) => ({ id, slot, name, source: slot === "executor" ? "codex_exec" : "claude_cli", default: { model: "cfg", effort: null }, models: [{ model: "cfg", efforts: [] }] });

function serve({ nodeRoles, flowRoles, judgeLimit = "3" }: { nodeRoles: ReturnType<typeof nodeRole>[]; flowRoles?: Record<string, string[]>; judgeLimit?: string }) {
  fetchMock.mockImplementation(async (input, init) => {
    const path = String(input);
    if (init?.method === "POST" && path === "/api/tasks") return Response.json({ task_id: "t-new", run_ids: ["r1"], skipped: [] }, { status: 201 });
    if (path === "/api/nodes") return Response.json([{ node_id: NODE, online: true, caps: { node_version: "t", os: "macos", model_sources: [] }, devices: [], last_seen: "2026-11-24T00:00:00Z" }]);
    if (path.startsWith("/api/projects/hafa/cases")) return Response.json([{ code: "TC-1", title: "充值", status: "active", review_status: "approved", should_fail: false, blocked_reason: null }]);
    if (path === "/api/models") return Response.json({ roles: [configRole("cfg-exec", "executor", "配置文件里的执行员"), configRole("cfg-judge", "judge", "配置文件里的判定员")] });
    if (path.startsWith(`/api/nodes/${NODE}/roles`)) return Response.json({ online: true, mine: true, name_problem: null, roles: nodeRoles });
    if (path.startsWith("/api/configs/flow_template") && flowRoles) {
      const item = { id: 1, name: "研发测试流程", code: "rd_test", value: JSON.stringify(flowRoles), label: "", group_name: "", sort: 1, remark: "", status: "normal", updated_at: "2026-11-24T00:00:00Z", updated_by: null };
      return Response.json({ type: { id: 1, name: "流程模板", code: "flow_template", remark: "", status: "normal", item_count: 1, updated_at: "2026-11-24T00:00:00Z", updated_by: null }, can_write: false, items: { total: 1, page: 1, page_size: 100, can_write: false, rows: [item] } });
    }
    if (path.startsWith("/api/configs/role_limit")) {
      const row = { id: 1, name: "判定员", code: "judge", value: judgeLimit, label: "", group_name: "", sort: 1, remark: "", status: "normal", updated_at: "2026-11-24T00:00:00Z", updated_by: null };
      return Response.json({ type: { id: 1, name: "岗位角色上限", code: "role_limit", remark: "", status: "normal", item_count: 1, updated_at: "2026-11-24T00:00:00Z", updated_by: null }, can_write: false, items: { total: 1, page: 1, page_size: 100, can_write: false, rows: [row] } });
    }
    return new Response("没有这个接口", { status: 404 });
  });
}

const posts = () => fetchMock.mock.calls.filter(([url, init]) => init?.method === "POST" && String(url) === "/api/tasks");

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  rememberUserName("小明");
  rememberReviewToken("tok-1234");
});
afterEach(() => { vi.unstubAllGlobals(); fetchMock.mockReset(); localStorage.clear(); rememberReviewToken(""); });

async function chooseNode(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("combobox", { name: "执行节点" }));
  await user.click(await screen.findByTitle(/aaaaaaaa · 在线/));
}

test("按所选节点上的角色选，不用配置文件里的；按流程自动选好执行员、主判定员和其他判定员，发出去的请求是对的", async () => {
  serve({
    nodeRoles: [nodeRole("e1", "executor", "牛马测试"), nodeRole("j1", "judge", "找茬", "ok", "opus"), nodeRole("j2", "judge", "挑刺", "ok", "sonnet"), nodeRole("j3", "judge", "没检测的", "unchecked")],
    flowRoles: { executor: ["牛马测试"], judge: ["找茬", "挑刺", "没检测的", "已删掉的"] },
  });
  const user = userEvent.setup();
  render(<NewTaskDialog onClose={() => {}} initialCases={["TC-1"]} />);
  await chooseNode(user);
  // 流程自动生效：没通过检测的、节点上没有的，如实说出来
  expect(await screen.findByText(/流程里的判定员「没检测的」还没通过检测/)).toBeInTheDocument();
  expect(screen.getByText(/流程里的判定员「已删掉的」在这个节点上不存在/)).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: /发起任务/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  const body = JSON.parse(String(posts()[0][1]?.body));
  expect(body).toMatchObject({
    node_id: NODE,
    case_codes: ["TC-1"],
    executor: { role: "e1", model: "m" },
    judge: { role: "j1", model: "opus" },
    more_judges: [{ role: "j2", model: "sonnet" }],
  });
  expect(JSON.stringify(body)).not.toContain("cfg-");
}, 20000);

test("判定员上限是 1 时，流程里多出来的判定员不选，并说明", async () => {
  serve({ nodeRoles: [nodeRole("e1", "executor", "牛马测试"), nodeRole("j1", "judge", "找茬"), nodeRole("j2", "judge", "挑刺")], flowRoles: { executor: ["牛马测试"], judge: ["找茬", "挑刺"] }, judgeLimit: "1" });
  const user = userEvent.setup();
  render(<NewTaskDialog onClose={() => {}} initialCases={["TC-1"]} />);
  await chooseNode(user);
  expect(await screen.findByText(/判定员最多同时 1 个，流程里多出来的没有选/)).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: /发起任务/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(JSON.parse(String(posts()[0][1]?.body)).more_judges).toBeUndefined();
});

test("没有配流程：手动选角色，没通过检测的角色置灰不能选", async () => {
  serve({ nodeRoles: [nodeRole("e1", "executor", "牛马测试"), nodeRole("j1", "judge", "找茬"), nodeRole("j2", "judge", "没检测的", "unchecked")] });
  const user = userEvent.setup();
  render(<NewTaskDialog onClose={() => {}} initialCases={["TC-1"]} />);
  await chooseNode(user);
  expect(screen.queryByRole("combobox", { name: "流程" })).not.toBeInTheDocument();
  await user.click(await screen.findByRole("combobox", { name: "判定员角色" }));
  const option = await screen.findByTitle("没检测的（还没通过检测）");
  expect(option.getAttribute("aria-disabled") ?? option.className).toMatch(/true|disabled/);
});

test("节点没上报角色（旧版节点）：回退用配置文件里的角色", async () => {
  serve({ nodeRoles: [] });
  const user = userEvent.setup();
  render(<NewTaskDialog onClose={() => {}} initialCases={["TC-1"]} />);
  await chooseNode(user);
  expect(await screen.findByText(/没有读到这个节点上的角色，先用配置文件里的角色/)).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: /发起任务/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(JSON.parse(String(posts()[0][1]?.body))).toMatchObject({ executor: { role: "cfg-exec" }, judge: { role: "cfg-judge" } });
  void within;
});

test("传入 initialNode 且节点在线时自动选中该节点（一键重跑场景）", async () => {
  serve({ nodeRoles: [nodeRole("e1", "executor", "牛马测试"), nodeRole("j1", "judge", "找茬")] });
  const user = userEvent.setup();
  render(<NewTaskDialog onClose={() => {}} initialCases={["TC-1"]} initialNode={NODE} />);
  // 不手动选节点，直接发起即可带上原节点与预选用例
  await waitFor(() => expect(screen.getByRole("button", { name: /发起任务/ })).not.toBeDisabled());
  await user.click(screen.getByRole("button", { name: /发起任务/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(JSON.parse(String(posts()[0][1]?.body))).toMatchObject({ node_id: NODE, case_codes: ["TC-1"] });
});

test("任务详情一键重跑筛选：排除通过、进行中和已跳过，保留失败/待确认/受阻/取消且去重", () => {
  const got = retryableRunCodes([
    { code: "A", status: "finished", final_result: "passed", blocked_reason: null },
    { code: "B", status: "finished", final_result: "failed", blocked_reason: null },
    { code: "B", status: "finished", final_result: "failed", blocked_reason: null },
    { code: "C", status: "finished", final_result: "uncertain", blocked_reason: null },
    { code: "D", status: "blocked", final_result: "blocked", blocked_reason: "link" },
    { code: "E", status: "blocked", final_result: "blocked", blocked_reason: "skipped" },
    { code: "F", status: "cancelled", final_result: null, blocked_reason: null },
    { code: "G", status: "running", final_result: null, blocked_reason: null },
  ]);
  expect(got).toEqual(["B", "C", "D", "F"]);
});
