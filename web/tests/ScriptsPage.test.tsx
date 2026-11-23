import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { NodeView, RegressionRequest, ScriptRow, Triage } from "../src/api";
import { rememberReviewToken } from "../src/reviewLogic";
import ScriptsPage from "../src/ScriptsPage";

const fetchMock = vi.fn<typeof fetch>();
const NODE_ONLINE = "aaaaaaaa-0000-0000-0000-000000000001";
const NODE_OFFLINE = "bbbbbbbb-0000-0000-0000-000000000002";

function script(overrides: Partial<ScriptRow> & Pick<ScriptRow, "id" | "status">): ScriptRow {
  return {
    case_code: "TC-04", case_title: "滑动并添加歌曲", version: 1, case_version: 1, source_run_id: "11111111-2222-3333-4444-555555555555",
    file_names: ["test_tc04.py"], gen_source: "claude_cli", gen_model: "m", gen_effort: "high", notes: "", status_note: null,
    last_regression: null, last_request: null, last_triage: null, healed_from: null,
    created_at: "2026-11-22T00:00:00Z", updated_at: "2026-11-22T00:00:00Z", ...overrides,
  };
}

const regression = (passed: boolean, id = "reg-1") => ({ id, passed, serial: "DEV0", detail: "输出", duration_ms: 1200, created_at: "2026-11-22T01:00:00Z" });
const request = (status: RegressionRequest["status"], detail = ""): RegressionRequest => ({ id: "req-1", node_id: NODE_ONLINE, status, detail, requested_at: "2026-11-22T02:00:00Z" });
const triage = (verdict: Triage["verdict"], extra: Partial<Triage> = {}): Triage => ({ regression_id: "reg-1", verdict, reason: "依据：控件文案变了", healed_script_id: null, healed_version: null, created_at: "2026-11-22T03:00:00Z", ...extra });

const nodes: NodeView[] = [
  { node_id: NODE_ONLINE, online: true, caps: { node_version: "t", os: "macos", model_sources: [] }, devices: [], last_seen: "2026-11-22T00:00:00Z" },
  { node_id: NODE_OFFLINE, online: false, caps: { node_version: "t", os: "macos", model_sources: [] }, devices: [], last_seen: "2026-11-22T00:00:00Z" },
];

/** 假服务端：脚本列表和节点列表；写请求记下来，回复由各测试指定 */
function serve(rows: ScriptRow[], write: () => Response = () => new Response('{"id":"req-1"}', { status: 201 })) {
  fetchMock.mockImplementation(async (input, init) => {
    const path = String(input);
    if (init?.method === "POST") return write();
    if (path.endsWith("/scripts")) return Response.json(rows);
    if (path === "/api/nodes") return Response.json(nodes);
    return new Response("没有这个接口", { status: 404 });
  });
}

const posts = () => fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  rememberReviewToken("");
});
afterEach(() => {
  vi.unstubAllGlobals();
  fetchMock.mockReset();
});

/** 一行里的业务按钮文字（不含表格自带的展开图标） */
const actionsIn = (row: HTMLElement) =>
  within(row).queryAllByRole("button").filter((b) => !b.classList.contains("ant-table-row-expand-icon")).map((b) => b.textContent?.replace(/\s/g, ""));

/** 页面里某一行（以用例编号加版本定位） */
async function rowOf(version: number) {
  const cell = await screen.findAllByText(`v${version}`, { selector: ".mono" });
  return cell[0].closest("tr") as HTMLElement;
}

test("按脚本状态只显示该状态能做的按钮", async () => {
  serve([
    script({ id: "s1", version: 1, status: "candidate" }),
    script({ id: "s2", version: 2, status: "self_check_passed" }),
    script({ id: "s3", version: 3, status: "approved" }),
    script({ id: "s4", version: 4, status: "rejected" }),
    script({ id: "s5", version: 5, status: "retired" }),
  ]);
  render(<ScriptsPage />);
  const buttons = async (version: number) => actionsIn(await rowOf(version));
  expect(await buttons(1)).toEqual(["拒绝"]);
  expect(await buttons(2)).toEqual(["获准", "拒绝"]);
  expect(await buttons(3)).toEqual(["回归", "下线"]);
  expect(await buttons(4)).toEqual([]);
  expect(await buttons(5)).toEqual([]);
});

test("回归请求排队中只能取消，运行中什么都不能点，结束后又能回归", async () => {
  for (const [status, expected] of [["queued", ["取消回归", "下线"]], ["running", ["下线"]], ["finished", ["回归", "下线"]], ["failed", ["回归", "下线"]]] as const) {
    serve([script({ id: "s3", version: 3, status: "approved", last_request: request(status) })]);
    const { unmount } = render(<ScriptsPage />);
    const row = await rowOf(3);
    expect(actionsIn(row), status).toEqual(expected);
    unmount();
  }
});

test("最近回归列：排队与运行中显示请求状态，结束后显示回归结果，没得到结论显示原因", async () => {
  serve([
    script({ id: "s1", version: 1, status: "approved", last_request: request("queued") }),
    script({ id: "s2", version: 2, status: "approved", last_regression: regression(true), last_request: request("finished") }),
    script({ id: "s3", version: 3, status: "approved", last_regression: regression(false) }),
    script({ id: "s4", version: 4, status: "approved", last_request: request("failed", "手机没连上") }),
    script({ id: "s5", version: 5, status: "approved" }),
  ]);
  render(<ScriptsPage />);
  expect(within(await rowOf(1)).getByText("排队，等节点领取")).toBeInTheDocument();
  expect(within(await rowOf(2)).getByText("回归通过")).toBeInTheDocument();
  expect(within(await rowOf(3)).getByText("回归失败")).toBeInTheDocument();
  const failed = await rowOf(4);
  expect(within(failed).getByText("没得到结论")).toBeInTheDocument();
  expect(within(failed).getByText("手机没连上")).toBeInTheDocument();
  expect(within(failed).queryByText("回归失败")).not.toBeInTheDocument();
  expect(within(await rowOf(5)).getByText("还没回归过")).toBeInTheDocument();
});

test("分诊标签只挂在它针对的那次失败上；修复版显示修自哪个版本", async () => {
  serve([
    script({ id: "old", version: 1, status: "approved", last_regression: regression(false, "reg-1"), last_triage: triage("locator_broken", { healed_script_id: "new", healed_version: 2 }) }),
    script({ id: "new", version: 2, status: "candidate", healed_from: "old" }),
    script({ id: "b", version: 3, status: "approved", last_regression: regression(false, "reg-1"), last_triage: triage("business_bug") }),
    script({ id: "u", version: 4, status: "approved", last_regression: regression(false, "reg-1"), last_triage: triage("unsure") }),
    // 分诊针对的是更早的失败：之后又回归失败了，标签不能还挂在新的失败上
    script({ id: "stale", version: 5, status: "approved", last_regression: regression(false, "reg-2"), last_triage: triage("business_bug") }),
    // 回归通过了，不显示分诊标签
    script({ id: "ok", version: 6, status: "approved", last_regression: regression(true, "reg-1"), last_triage: triage("business_bug") }),
  ]);
  render(<ScriptsPage />);
  expect(within(await rowOf(1)).getByText("定位失效，已产出修复候选 v2")).toBeInTheDocument();
  expect(within(await rowOf(2)).getByText("修自 v1")).toBeInTheDocument();
  expect(within(await rowOf(3)).getByText("疑似产品缺陷，待人处理")).toBeInTheDocument();
  expect(within(await rowOf(4)).getByText("分不清，待人判断")).toBeInTheDocument();
  expect(within(await rowOf(5)).queryByText("疑似产品缺陷，待人处理")).not.toBeInTheDocument();
  expect(within(await rowOf(6)).queryByText("疑似产品缺陷，待人处理")).not.toBeInTheDocument();
});

async function openRegressDialog(user: ReturnType<typeof userEvent.setup>) {
  await user.click(within(await rowOf(3)).getByRole("button", { name: /回\s*归/ }));
  return screen.findByRole("dialog");
}

test("发起回归：选在线节点、填个人令牌、再输审核口令，POST 带齐令牌和节点", async () => {
  serve([script({ id: "s3", version: 3, status: "approved" })]);
  const user = userEvent.setup();
  render(<ScriptsPage />);
  const dialog = await openRegressDialog(user);

  await user.click(within(dialog).getByRole("combobox"));
  const offline = await screen.findByText(/bbbbbbbb · 离线/);
  expect(offline.closest(".ant-select-item")).toHaveAttribute("aria-disabled", "true");
  await user.click(await screen.findByText(/aaaaaaaa · 在线/));
  await user.type(within(dialog).getByLabelText("个人令牌"), "u_personal-1");
  await user.click(within(dialog).getByRole("button", { name: /排队回归/ }));

  // 没有审核口令：先弹口令框，不能直接发
  expect(posts()).toHaveLength(0);
  const tokenDialog = await screen.findByRole("dialog", { name: "输入审核口令" });
  await user.type(within(tokenDialog).getByLabelText("审核口令"), "review-token-1");
  await user.click(within(tokenDialog).getByRole("button", { name: /确\s*认/ }));

  await waitFor(() => expect(posts()).toHaveLength(1));
  const [path, init] = posts()[0];
  expect(String(path)).toBe("/api/scripts/s3/regress");
  const headers = new Headers(init?.headers);
  expect([headers.get("x-review-token"), headers.get("x-user-token")]).toEqual(["review-token-1", "u_personal-1"]);
  expect(JSON.parse(String(init?.body))).toEqual({ node_id: NODE_ONLINE });
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
});

test("发起回归：没选节点或没填个人令牌就不发请求，并说明缺什么", async () => {
  serve([script({ id: "s3", version: 3, status: "approved" })]);
  const user = userEvent.setup();
  render(<ScriptsPage />);
  const dialog = await openRegressDialog(user);
  await user.click(within(dialog).getByRole("button", { name: /排队回归/ }));
  expect(await within(dialog).findByText("请选择在线节点")).toBeInTheDocument();
  await user.click(within(dialog).getByRole("combobox"));
  await user.click(await screen.findByText(/aaaaaaaa · 在线/));
  await user.click(within(dialog).getByRole("button", { name: /排队回归/ }));
  expect(await within(dialog).findByText("请输入个人令牌")).toBeInTheDocument();
  expect(posts()).toHaveLength(0);
});

test("发起回归被服务端拒绝：弹窗里显示服务端原因，不关闭，不冒充口令问题", async () => {
  rememberReviewToken("review-token-1");
  serve([script({ id: "s3", version: 3, status: "approved" })], () => new Response("只有已获准的脚本版本能回归，这个版本现在是「retired」", { status: 409 }));
  const user = userEvent.setup();
  render(<ScriptsPage />);
  const dialog = await openRegressDialog(user);
  await user.click(within(dialog).getByRole("combobox"));
  await user.click(await screen.findByText(/aaaaaaaa · 在线/));
  await user.type(within(dialog).getByLabelText("个人令牌"), "u_personal-1");
  await user.click(within(dialog).getByRole("button", { name: /排队回归/ }));
  expect(await within(dialog).findByText(/只有已获准的脚本版本能回归/)).toBeInTheDocument();
  expect(screen.queryByRole("dialog", { name: "输入审核口令" })).not.toBeInTheDocument();
  expect(posts()).toHaveLength(1);
});

test("取消回归：不用选节点，请求编号进路径", async () => {
  rememberReviewToken("review-token-1");
  serve([script({ id: "s3", version: 3, status: "approved", last_request: request("queued") })], () => new Response(null, { status: 204 }));
  const user = userEvent.setup();
  render(<ScriptsPage />);
  await user.click(within(await rowOf(3)).getByRole("button", { name: /取消回归/ }));
  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).queryByRole("combobox")).not.toBeInTheDocument();
  await user.type(within(dialog).getByLabelText("个人令牌"), "u_personal-1");
  await user.click(within(dialog).getByRole("button", { name: /确认取消/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(String(posts()[0][0])).toBe("/api/regression-requests/req-1/cancel");
});

test("连不上服务端时显示加载错误而不是空表", async () => {
  fetchMock.mockRejectedValue(new TypeError("fetch failed"));
  render(<ScriptsPage />);
  expect(await screen.findByText("连不上平台服务端")).toBeInTheDocument();
});

test("项目里还没有脚本时给出引导", async () => {
  serve([]);
  render(<ScriptsPage />);
  expect(await screen.findByText(/还没有脚本/)).toBeInTheDocument();
});
