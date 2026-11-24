import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { NodeView, RoleView } from "../src/api";
import { describeFlowRoles, parseFlowRoles } from "../src/api";
import FlowTemplateEditor from "../src/FlowTemplateEditor";

const fetchMock = vi.fn<typeof fetch>();
const NODE = "aaaaaaaa-0000-0000-0000-000000000001";
const nodes: NodeView[] = [{ node_id: NODE, online: true, caps: { node_version: "t", os: "macos", model_sources: [] }, devices: [], last_seen: "2026-11-24T00:00:00Z" }];
const role = (id: string, slot: string, name: string): RoleView =>
  ({ id, slot, name, source: "claude_cli", default: { model: "opus", effort: null }, models: [], key_configured: false, check: { status: "ok", detail: null, checked_at: null } }) as RoleView;

function serve(roles: RoleView[], limits: Record<string, string> = {}) {
  fetchMock.mockImplementation(async (input) => {
    const path = String(input);
    if (path === "/api/nodes") return Response.json(nodes);
    if (path.startsWith(`/api/nodes/${NODE}/roles`)) return Response.json({ online: true, mine: true, name_problem: null, roles });
    if (path.startsWith("/api/configs/role_limit")) {
      const rows = Object.entries(limits).map(([code, value], i) => ({ id: i + 1, name: code, code, value, label: "", group_name: "", sort: i, remark: "", status: "normal", updated_at: "2026-11-24T00:00:00Z", updated_by: null }));
      return Response.json({ type: { id: 1, name: "岗位角色上限", code: "role_limit", remark: "", status: "normal", item_count: rows.length, updated_at: "2026-11-24T00:00:00Z", updated_by: null }, can_write: false, items: { total: rows.length, page: 1, page_size: 100, can_write: false, rows } });
    }
    return new Response("没有这个接口", { status: 404 });
  });
}

function Harness({ initial = "" }: { initial?: string }) {
  const [value, setValue] = useState(initial);
  return (
    <>
      <FlowTemplateEditor value={value} onChange={setValue} />
      <output data-testid="value">{value}</output>
    </>
  );
}
const currentValue = () => screen.getByTestId("value").textContent ?? "";

beforeEach(() => vi.stubGlobal("fetch", fetchMock));
afterEach(() => { vi.unstubAllGlobals(); fetchMock.mockReset(); localStorage.clear(); });

test("只列出节点上有的角色，勾选后生成按岗位分组的角色名列表", async () => {
  serve([role("1", "case_writer", "用例甲"), role("2", "case_writer", "用例乙"), role("3", "judge", "找茬")]);
  const user = userEvent.setup();
  render(<Harness />);
  await user.click(await screen.findByRole("checkbox", { name: /用例甲/ }));
  await user.click(screen.getByRole("checkbox", { name: /找茬/ }));
  await user.click(screen.getByRole("checkbox", { name: /用例乙/ }));
  expect(JSON.parse(currentValue())).toEqual({ judge: ["找茬"], case_writer: ["用例甲", "用例乙"] });
  // 节点上没有的岗位（执行员、管理员、UI 走查）不显示
  expect(screen.queryByText("执行员")).not.toBeInTheDocument();
  await user.click(screen.getByRole("checkbox", { name: /用例甲/ }));
  expect(JSON.parse(currentValue()).case_writer).toEqual(["用例乙"]);
});

test("每个岗位最多勾几个由岗位角色上限决定：满了其余的置灰，调大后能多选（UI 走查也一样）", async () => {
  serve([1, 2, 3, 4].map((n) => role(String(n), "ui_review", `走查${n}`)).concat([role("9", "judge", "判定1"), role("10", "judge", "判定2")]), { ui_review: "4", judge: "1" });
  const user = userEvent.setup();
  render(<Harness />);
  // UI 走查上限 4：4 个都能勾
  for (const n of [1, 2, 3, 4]) await user.click(await screen.findByRole("checkbox", { name: new RegExp(`走查${n}`) }));
  expect(JSON.parse(currentValue()).ui_review).toEqual(["走查1", "走查2", "走查3", "走查4"]);
  // 判定员上限 1：勾了一个，另一个置灰
  await user.click(screen.getByRole("checkbox", { name: /判定1/ }));
  expect(screen.getByRole("checkbox", { name: /判定2/ })).toBeDisabled();
  expect(screen.getByText(/已选满 1 个/)).toBeInTheDocument();
});

test("值里已有、节点上没有的角色单独列出，取消勾选就去掉；读不到上限时用默认 3", async () => {
  serve([role("1", "judge", "找茬")]);
  const user = userEvent.setup();
  render(<Harness initial='{"judge":["找茬","已删掉的判定员"],"case_writer":"旧格式用例"}' />);
  const missing = await screen.findByText("流程里有、但这个节点上没有的角色");
  const group = within(missing.closest(".flow-editor-group") as HTMLElement);
  expect(group.getByRole("checkbox", { name: /已删掉的判定员/ })).toBeChecked();
  expect(group.getByRole("checkbox", { name: /旧格式用例/ })).toBeChecked();
  await user.click(group.getByRole("checkbox", { name: /已删掉的判定员/ }));
  expect(JSON.parse(currentValue())).toEqual({ judge: ["找茬"], case_writer: ["旧格式用例"] });
  expect(screen.getByText(/\/ 上限 3/)).toBeInTheDocument();
});

test("数量选择：选 N 就是 N 个不同的角色，不够自动补、多了从后面去掉，不超过上限", async () => {
  serve([1, 2, 3, 4].map((n) => role(String(n), "ui_review", `走查${n}`)), { ui_review: "3" });
  const user = userEvent.setup();
  render(<Harness />);
  const pickCount = async (n: number) => {
    await user.click(await screen.findByRole("combobox", { name: "UI 走查数量" }));
    await user.click(await screen.findByTitle(String(n)));
  };
  await pickCount(3);
  expect(JSON.parse(currentValue()).ui_review).toEqual(["走查1", "走查2", "走查3"]);
  // 3 个不同的角色，不是同一个角色 3 次
  expect(new Set(JSON.parse(currentValue()).ui_review).size).toBe(3);
  await pickCount(1);
  expect(JSON.parse(currentValue()).ui_review).toEqual(["走查1"]);
  // 手动再勾一个，数量同步；选 0 就整个岗位去掉
  await user.click(screen.getByRole("checkbox", { name: /走查4/ }));
  expect(JSON.parse(currentValue()).ui_review).toEqual(["走查1", "走查4"]);
  await pickCount(0);
  expect(currentValue()).toBe("{}");
  // 数量最多到上限 3（节点上有 4 个角色也不能选 4）
  await user.click(screen.getByRole("combobox", { name: "UI 走查数量" }));
  expect(screen.queryByTitle("4")).not.toBeInTheDocument();
});

test("读不到节点角色时给出原因，可以切到手写 JSON", async () => {
  fetchMock.mockImplementation(async () => new Response("服务端挂了", { status: 500 }));
  const user = userEvent.setup();
  render(<Harness initial='{"judge":["找茬"]}' />);
  await waitFor(() => expect(screen.getAllByText(/读取节点失败|服务端挂了|读不到角色/).length).toBeGreaterThan(0));
  await user.click(screen.getByRole("switch"));
  expect(screen.getByLabelText("流程 JSON")).toHaveValue('{"judge":["找茬"]}');
});

test("解析和显示：旧格式（单个名字）和新格式（名字列表）都认", () => {
  expect(parseFlowRoles('{"judge":"找茬","case_writer":["甲","乙","甲"]}')).toEqual({ judge: ["找茬"], case_writer: ["甲", "乙"] });
  expect(parseFlowRoles("不是json")).toBeNull();
  expect(parseFlowRoles("{}")).toBeNull();
  expect(describeFlowRoles({ ui_review: ["走查甲"], judge: ["找茬"], case_writer: ["甲", "乙"] })).toBe("判定员：找茬；测试用例：甲、乙；UI 走查：走查甲");
});
