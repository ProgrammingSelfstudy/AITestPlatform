import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { NodeView, RoleView } from "../src/api";
import GenerateDialog from "../src/GenerateDialog";
import { rememberReviewToken } from "../src/reviewLogic";

const fetchMock = vi.fn<typeof fetch>();
const NODE = "aaaaaaaa-0000-0000-0000-000000000001";
const nodes: NodeView[] = [{ node_id: NODE, online: true, caps: { node_version: "t", os: "macos", model_sources: [] }, devices: [], last_seen: "2026-11-24T00:00:00Z" }];

const role = (id: string, slot: string, name: string, source: string, status: "ok" | "unchecked"): RoleView =>
  ({ id, slot, name, source, default: { model: "m", effort: null }, models: [{ model: "m", efforts: [] }], key_configured: false, check: { status, detail: null, checked_at: null } }) as RoleView;

function serve(roles: RoleView[], flowValue: string | null) {
  fetchMock.mockImplementation(async (input) => {
    const path = String(input);
    if (path === "/api/nodes") return Response.json(nodes);
    if (path === `/api/nodes/${NODE}/roles`) return Response.json({ online: true, mine: true, name_problem: null, unowned: false, roles });
    if (path.startsWith("/api/configs/flow_template") && flowValue) {
      const item = { id: 1, name: "研发测试流程", code: "rd_test", value: flowValue, label: "", group_name: "", sort: 1, remark: "", status: "normal", updated_at: "2026-11-24T00:00:00Z", updated_by: null };
      return Response.json({ type: { id: 1, name: "流程模板", code: "flow_template", remark: "", status: "normal", item_count: 1, updated_at: "2026-11-24T00:00:00Z", updated_by: null }, can_write: false, items: { total: 1, page: 1, page_size: 100, can_write: false, rows: [item] } });
    }
    return new Response("没有这个接口", { status: 404 });
  });
}

beforeEach(() => vi.stubGlobal("fetch", fetchMock));
afterEach(() => { vi.unstubAllGlobals(); fetchMock.mockReset(); localStorage.clear(); rememberReviewToken(""); });

async function openAsOwner() {
  const user = userEvent.setup();
  render(<GenerateDialog onClose={() => {}} />);
  await user.type(await screen.findByPlaceholderText(/你的名字/), "小明");
  return user;
}

test("选了研发测试流程：测试用例角色和判定员角色按流程里的名字自动选好", async () => {
  serve(
    [role("r1", "case_writer", "牛马测试用例", "codex_exec", "ok"), role("r2", "case_writer", "别的用例角色", "claude_cli", "ok"), role("r3", "judge", "找茬", "claude_cli", "ok"), role("r4", "judge", "别的判定员", "agy", "ok")],
    JSON.stringify({ case_writer: "牛马测试用例", judge: "找茬" }),
  );
  await openAsOwner();
  // 默认就选了流程；角色下拉里显示的是流程指定的，不是列表里的第一个
  expect(await screen.findByText("研发测试流程")).toBeInTheDocument();
  expect(await screen.findByText(/^牛马测试用例（/)).toBeInTheDocument();
  expect(await screen.findByText(/^找茬（/)).toBeInTheDocument();
  // 没有被选中的角色不会显示在下拉框的已选项里
  expect(screen.queryByText(/^别的用例角色（/)).not.toBeInTheDocument();
  expect(screen.queryByText(/^别的判定员（/)).not.toBeInTheDocument();
  expect(screen.queryByText(/在这个节点上不存在/)).not.toBeInTheDocument();
});

test("流程里的角色在节点上没有：如实说出来；没检测的会自动检测", async () => {
  serve(
    [role("r1", "case_writer", "牛马测试用例", "codex_exec", "unchecked"), role("r3", "judge", "找茬", "claude_cli", "ok")],
    JSON.stringify({ case_writer: "牛马测试用例", judge: "不存在的判定员" }),
  );
  await openAsOwner();
  expect(await screen.findByText(/流程里的测试用例角色「牛马测试用例」正在(自动)?检测/)).toBeInTheDocument();
  expect(screen.getByText(/流程里的判定员角色「不存在的判定员」在这个节点上不存在/)).toBeInTheDocument();
});

/** 带「检测」接口的服务：检测请求发出后，下一次读角色就是「可用」 */
function serveWithCheck(roles: RoleView[], flowValue: string | null) {
  const checked: string[] = [];
  serve(roles, flowValue);
  const base = fetchMock.getMockImplementation()!;
  fetchMock.mockImplementation(async (input, init) => {
    const path = String(input);
    const hit = new RegExp(`^/api/nodes/${NODE}/roles/([^/]+)/check$`).exec(path);
    if (hit && init?.method === "POST") {
      checked.push(decodeURIComponent(hit[1]));
      return new Response(null, { status: 204 });
    }
    if (path === `/api/nodes/${NODE}/roles`) {
      const now = roles.map((r) => (checked.includes(r.id) ? { ...r, check: { status: "ok", detail: null, checked_at: "2026-11-25T00:00:00Z" } } : r));
      return Response.json({ online: true, mine: true, name_problem: null, unowned: false, roles: now });
    }
    return base(input, init);
  });
  return checked;
}

test("选了流程：流程里没检测的角色自动检测，检测通过后提示消失", async () => {
  rememberReviewToken("口令");
  const checked = serveWithCheck(
    [role("r1", "case_writer", "牛马测试用例", "codex_exec", "unchecked"), role("r3", "judge", "找茬", "claude_cli", "unchecked")],
    JSON.stringify({ case_writer: "牛马测试用例", judge: "找茬" }),
  );
  await openAsOwner();
  await waitFor(() => expect([...checked].sort()).toEqual(["r1", "r3"]));
  // 检测中的轮询每 2 秒读一次，读到可用后「正在自动检测」的提示就没了
  await waitFor(() => expect(screen.queryByText(/正在自动检测/)).not.toBeInTheDocument(), { timeout: 8000 });
  expect(screen.queryByText(/还没检测/)).not.toBeInTheDocument();
});

test("手动选一个没检测的角色：马上自动检测，不用再去角色页", async () => {
  rememberReviewToken("口令");
  const checked = serveWithCheck(
    [role("r1", "case_writer", "牛马测试用例", "codex_exec", "ok"), role("r2", "case_writer", "另一个用例角色", "claude_cli", "unchecked"), role("r3", "judge", "找茬", "claude_cli", "ok")],
    null,
  );
  const user = await openAsOwner();
  await screen.findByText(/^牛马测试用例（/);
  expect(checked).toEqual([]); // 已经可用的不重复检测
  const [writerSelect] = screen.getAllByRole("combobox").slice(-2);
  await user.click(writerSelect);
  await user.click(await screen.findByText("另一个用例角色", { selector: ".mod-role-option-name" }));
  await waitFor(() => expect(checked).toEqual(["r2"]));
});

test("没有配置流程模板时不显示流程选择，照常手动选角色", async () => {
  serve([role("r1", "case_writer", "牛马测试用例", "codex_exec", "ok")], null);
  await openAsOwner();
  expect(await screen.findByText(/^牛马测试用例（/)).toBeInTheDocument();
  expect(screen.queryByText("在「参数配置」里的流程模板中配置")).not.toBeInTheDocument();
});
