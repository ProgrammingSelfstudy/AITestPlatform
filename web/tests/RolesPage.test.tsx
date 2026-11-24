import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { NodeView, RoleView } from "../src/api";
import { NEED_LOGIN_MESSAGE, readUserToken } from "../src/identity";
import { rememberReviewToken } from "../src/reviewLogic";
import RolesPage from "../src/RolesPage";

const fetchMock = vi.fn<typeof fetch>();
const NODE = "aaaaaaaa-0000-0000-0000-000000000001";
/** 这个节点的主人（节点启动时的 NODE_USER_NAME）。身份只记名字，不是机密 */
const OWNER = "小明";
const SECRET_KEY = "sk-这个Key绝不能出现在页面里";

const nodes: NodeView[] = [{ node_id: NODE, online: true, caps: { node_version: "t", os: "macos", model_sources: [] }, devices: [], last_seen: "2026-11-22T00:00:00Z" }];

function role(id: string, slot: RoleView["slot"], source: string, status: RoleView["check"]["status"], detail: string | null, extra: Partial<RoleView> = {}): RoleView {
  return {
    id, slot, name: `角色-${id}`, source, default: { model: "m", effort: null }, models: [{ model: "m", efforts: [] }],
    key_configured: false, check: { status, detail, checked_at: status === "unchecked" ? null : "2026-11-22T10:00:00Z" }, ...extra,
  };
}

/** 请求头里的「我是谁」（百分号编码）；没带返回 null */
const nameOf = (init?: RequestInit): string | null => {
  const raw = (init?.headers as Record<string, string> | undefined)?.["x-user-name"];
  return raw ? decodeURIComponent(raw) : null;
};

function serve(roles: RoleView[], online = true) {
  fetchMock.mockImplementation(async (input, init) => {
    const path = String(input);
    if (init?.method === "POST") return new Response(null, { status: 202 });
    if (path === "/api/nodes") return Response.json(nodes);
    // 查看是公开的：谁都能看。mine：没填名字为 null；填了名字，是节点主人为 true，否则 false
    if (path === `/api/nodes/${NODE}/roles`) {
      const name = nameOf(init);
      return Response.json({ online, roles, mine: name === null ? null : name === OWNER, name_problem: null });
    }
    // 页面顶部的「选应用」面板也会读应用检查结果；这里不关心内容，给一个正常的空结果，免得它因为 404 反复报错重绘
    if (path === `/api/nodes/${NODE}/apps`) return Response.json({ online, mine: nameOf(init) === OWNER ? true : null, name_problem: null, apps: ["claude_cli", "codex_exec", "agy", "deepseek"].map((source) => ({ source, scanned: false })) });
    return new Response("没有这个接口", { status: 404 });
  });
}

async function openPage() {
  const user = userEvent.setup();
  render(<RolesPage />);
  await user.type(await screen.findByPlaceholderText(/你的名字/), OWNER);
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

test("没填名字也能看角色，但是只读：不能检查、不能创建，并说明为什么", async () => {
  serve([role("a", "executor", "codex_exec", "ok", "已用 ChatGPT 登录")]);
  render(<RolesPage />);
  // 数据公开：不填名字就能看到角色
  expect((await screen.findAllByText("角色-a")).length).toBeGreaterThan(0);
  expect(fetchMock.mock.calls.some(([input, init]) => String(input).includes("/roles") && nameOf(init) !== null)).toBe(false);
  // 「选应用」面板一进页面就能看到：四张卡片照常显示，说明只读的原因；点开也不会发检查请求
  for (const name of ["Claude", "Codex", "Gemini", "DeepSeek"]) expect(screen.getByText(name)).toBeInTheDocument();
  expect(screen.getAllByText(/你现在是只读/).length).toBeGreaterThan(0);
  await userEvent.setup().click(screen.getByText("Codex").closest("button")!);
  expect(fetchMock.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
});

test("填的是别人的名字：能看，但不能改（节点是别人的，用的是他的额度）", async () => {
  serve([role("a", "executor", "codex_exec", "ok", null)]);
  const user = userEvent.setup();
  render(<RolesPage />);
  await user.type(await screen.findByPlaceholderText(/你的名字/), "路人甲");
  expect((await screen.findAllByText("角色-a")).length).toBeGreaterThan(0);
  expect((await screen.findAllByText(/这是别人的节点/)).length).toBeGreaterThan(0);
  expect(screen.getByRole("button", { name: /检\s*测/ })).toBeDisabled();
});

test("服务端提示先登录时可输入密码，登录后刷新权限，清除时注销", async () => {
  const password = crypto.randomUUID();
  const token = "ab".repeat(32);
  const roles = [role("a", "executor", "codex_exec", "ok", null)];
  fetchMock.mockImplementation(async (path, init) => {
    if (path === "/api/nodes") return Response.json(nodes);
    if (path === "/api/login") {
      const body = JSON.parse(String(init?.body));
      return body.name === OWNER && body.password === password
        ? Response.json({ name: OWNER, token })
        : new Response("名字或密码不对", { status: 403 });
    }
    if (path === "/api/logout") return new Response(null, { status: 204 });
    const owner = nameOf(init) === OWNER;
    const loggedIn = owner && new Headers(init?.headers).get("x-user-token") === token;
    const viewer = { online: true, mine: owner ? loggedIn : null, name_problem: owner && !loggedIn ? NEED_LOGIN_MESSAGE : null };
    if (path === `/api/nodes/${NODE}/roles`) return Response.json({ ...viewer, roles });
    return Response.json({ ...viewer, apps: [] });
  });

  const user = await openPage();
  expect((await screen.findAllByText("角色-a")).length).toBeGreaterThan(0);
  expect(screen.getByRole("button", { name: /检\s*测/ })).toBeDisabled();
  await user.click(await screen.findByRole("button", { name: "登录" }));
  const dialog = within(await screen.findByRole("dialog", { name: "登录" }));
  expect(dialog.getByText(`名字 ${OWNER}`)).toBeInTheDocument();
  const input = dialog.getByLabelText("密码");
  const login = dialog.getByRole("button", { name: /^登\s*录$/ });
  await user.type(input, crypto.randomUUID());
  await user.click(login);
  expect(await dialog.findByRole("alert")).toHaveTextContent("名字或密码不对");
  expect(input).toHaveValue("");

  await user.type(input, password);
  await user.click(login);
  await waitFor(() => expect(screen.getByRole("button", { name: /检\s*测/ })).toBeEnabled());
  expect(readUserToken(OWNER) === token).toBe(true);
  await user.click(screen.getByRole("button", { name: "清除" }));
  expect(readUserToken(OWNER)).toBe("");
  const logout = fetchMock.mock.calls.find(([path]) => path === "/api/logout");
  expect(logout?.[1]?.method).toBe("POST");
  expect(new Headers(logout?.[1]?.headers).get("x-user-token") === token).toBe(true);
});

test("每个检测状态都有文字，失败原因原样显示，Key 永远不出现", { timeout: 20000 }, async () => {
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

test("节点还没有角色时可以用模板初始化，请求带「我是谁」、不带 Key", async () => {
  rememberReviewToken("synthetic-review-token-012345");
  serve([]);
  const user = await openPage();
  await user.click(await screen.findByRole("button", { name: /用平台模板初始化/ }));
  await waitFor(() => expect(fetchMock.mock.calls.some(([input, init]) => init?.method === "POST" && String(input) === `/api/nodes/${NODE}/roles/init-template`)).toBe(true));
  const [, init] = fetchMock.mock.calls.find(([input]) => String(input).endsWith("/init-template"))!;
  expect(String(init?.body)).not.toContain("api_key");
  expect(nameOf(init)).toBe(OWNER);
});

test("「我是谁」填了记在浏览器里：刷新（重新进入页面）后不用再填，也能清除", { timeout: 30000 }, async () => {
  serve([role("a", "executor", "codex_exec", "ok", null)]);
  const user = userEvent.setup();
  const first = render(<RolesPage />);
  await user.type(await screen.findByPlaceholderText(/你的名字/), OWNER);
  await screen.findAllByText("角色-a");
  expect(screen.getByText(/已记住，到期/)).toBeInTheDocument();
  // 重新进入页面：名字还在，请求直接带上，不用再填
  first.unmount();
  fetchMock.mockClear();
  render(<RolesPage />);
  expect((await screen.findAllByText("角色-a")).length).toBeGreaterThan(0);
  expect(fetchMock.mock.calls.some(([input, init]) => String(input).includes("/roles") && nameOf(init) === OWNER)).toBe(true);
  // 清除：回到只读
  await user.click(screen.getByRole("button", { name: "清除" }));
  expect((await screen.findAllByText(/你现在是只读/)).length).toBeGreaterThan(0);
  expect(window.localStorage.getItem("platform.userName.v1")).toBeNull();
});

test("可以连续新建多个角色，每个都显示在列表里（比如执行员和判定员一起建）", async () => {
  serve([role("a", "executor", "codex_exec", "ok", null)]);
  const user = await openPage();
  const add = await screen.findByRole("button", { name: /新增角色/ });
  await waitFor(() => expect(add).toBeEnabled());
  await user.click(add);
  await user.click(add);
  // 两个新建草稿都在列表里，而且按钮不会变成「继续编辑」把第二次点击吞掉
  expect(await screen.findAllByText("新建未保存")).toHaveLength(2);
  expect(screen.getAllByText("未命名新角色")).toHaveLength(2);
  expect(screen.queryByText("继续编辑新建角色")).toBeNull();
});
