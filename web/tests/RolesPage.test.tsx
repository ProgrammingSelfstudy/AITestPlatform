import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { NodeView, RoleView } from "../src/api";
import { rememberReviewToken } from "../src/reviewLogic";
import RolesPage from "../src/RolesPage";

const fetchMock = vi.fn<typeof fetch>();
const NODE = "aaaaaaaa-0000-0000-0000-000000000001";
const USER_TOKEN = "u_synthetic-token-for-tests";
const SECRET_KEY = "sk-这个Key绝不能出现在页面里";

const nodes: NodeView[] = [{ node_id: NODE, online: true, caps: { node_version: "t", os: "macos", model_sources: [] }, devices: [], last_seen: "2026-11-22T00:00:00Z" }];

function role(id: string, slot: RoleView["slot"], source: string, status: RoleView["check"]["status"], detail: string | null, extra: Partial<RoleView> = {}): RoleView {
  return {
    id, slot, name: `角色-${id}`, source, default: { model: "m", effort: null }, models: [{ model: "m", efforts: [] }],
    key_configured: false, check: { status, detail, checked_at: status === "unchecked" ? null : "2026-11-22T10:00:00Z" }, ...extra,
  };
}

function serve(roles: RoleView[], online = true) {
  fetchMock.mockImplementation(async (input, init) => {
    const path = String(input);
    if (init?.method === "POST") return new Response(null, { status: 202 });
    if (path === "/api/nodes") return Response.json(nodes);
    if (path === `/api/nodes/${NODE}/roles`) return Response.json({ online, roles });
    return new Response("没有这个接口", { status: 404 });
  });
}

async function openPage() {
  const user = userEvent.setup();
  render(<RolesPage />);
  await user.type(await screen.findByPlaceholderText(/个人令牌/), USER_TOKEN);
  return user;
}

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  rememberReviewToken("");
});
afterEach(() => {
  vi.unstubAllGlobals();
  fetchMock.mockReset();
});

test("没填个人令牌时不请求角色", async () => {
  serve([]);
  render(<RolesPage />);
  await screen.findByText(/请输入个人令牌/);
  expect(fetchMock.mock.calls.some(([input]) => String(input).includes("/roles"))).toBe(false);
});

test("每个检测状态都有文字，失败原因原样显示，Key 永远不出现", async () => {
  serve([
    role("a", "executor", "codex_exec", "ok", "已用 ChatGPT 登录"),
    role("b", "judge", "agy", "quota_exhausted", "agy 额度用完或被限流"),
    role("c", "judge", "claude_cli", "not_logged_in", "未检测到 Claude 会员登录"),
    role("d", "judge", "claude_cli", "failed", "原始错误：连接被拒绝"),
    role("e", "judge", "agy", "unchecked", null, { key_configured: true }),
  ]);
  await openPage();
  await screen.findAllByText("角色-a");
  for (const text of ["可用", "额度用完", "未登录", "检测失败", "未检测"]) {
    expect((await screen.findAllByText(text)).length, `缺少状态文字「${text}」`).toBeGreaterThan(0);
  }
  expect(screen.getAllByText("原始错误：连接被拒绝").length).toBeGreaterThan(0);
  expect(document.body.textContent).not.toContain(SECRET_KEY);
  // 已配置 Key 的角色只显示「已配置」，不会有任何 Key 输入框带值
  for (const input of document.querySelectorAll("input")) expect((input as HTMLInputElement).value).not.toContain("sk-");
});

test("节点离线时明确提示，不显示角色", async () => {
  serve([role("a", "executor", "codex_exec", "ok", null)], false);
  await openPage();
  expect((await screen.findAllByText(/节点离线，无法读取和修改角色/)).length).toBeGreaterThan(0);
  expect(screen.queryByText("角色-a")).not.toBeInTheDocument();
});

test("节点还没有角色时可以用模板初始化，请求不带 Key", async () => {
  rememberReviewToken("synthetic-review-token-012345");
  serve([]);
  const user = await openPage();
  await user.click(await screen.findByRole("button", { name: /用平台模板初始化/ }));
  await waitFor(() => expect(fetchMock.mock.calls.some(([input, init]) => init?.method === "POST" && String(input) === `/api/nodes/${NODE}/roles/init-template`)).toBe(true));
  const [, init] = fetchMock.mock.calls.find(([input]) => String(input).endsWith("/init-template"))!;
  expect(String(init?.body)).not.toContain("api_key");
  expect((init?.headers as Record<string, string>)["x-user-token"]).toBe(USER_TOKEN);
});
