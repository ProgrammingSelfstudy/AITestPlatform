import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { ConfigItem, ConfigType } from "../src/api";
import ConfigDetailPage from "../src/ConfigDetailPage";
import ConfigsPage from "../src/ConfigsPage";

const fetchMock = vi.fn<typeof fetch>();

const type = (over: Partial<ConfigType> = {}): ConfigType => ({
  id: 1, name: "岗位角色上限", code: "role_limit", remark: "每个岗位最多几个角色", status: "normal", item_count: 2, updated_at: "2026-11-23T00:00:00Z", updated_by: null, ...over,
});
const item = (over: Partial<ConfigItem> = {}): ConfigItem => ({
  id: 11, name: "执行员", code: "executor", value: "3", label: "个", group_name: "role", sort: 1, remark: "", status: "normal", updated_at: "2026-11-23T00:00:00Z", updated_by: null, ...over,
});

beforeEach(() => vi.stubGlobal("fetch", fetchMock));
afterEach(() => { vi.unstubAllGlobals(); fetchMock.mockReset(); });

test("一级列表：编码是链接，写操作按钮禁用，搜索把条件带给接口", async () => {
  fetchMock.mockImplementation(async () => Response.json({ total: 1, page: 1, page_size: 10, rows: [type()], can_write: false }));
  const user = userEvent.setup();
  render(<ConfigsPage />);
  const link = await screen.findByRole("link", { name: "role_limit" });
  expect(link).toHaveAttribute("href", "#/configs/role_limit");
  expect(screen.getByRole("button", { name: /新\s?增/ })).toBeDisabled();
  expect(screen.getByRole("button", { name: /导\s?入/ })).toBeDisabled();
  await user.type(screen.getByPlaceholderText("请输入类型编码"), "role");
  await user.click(screen.getByRole("button", { name: /搜\s?索/ }));
  await waitFor(() => expect(fetchMock.mock.calls.some(([url]) => String(url).includes("code=role"))).toBe(true));
});

test("一级列表：加载失败显示原因", async () => {
  fetchMock.mockImplementation(async () => new Response("查询配置类型失败", { status: 500 }));
  render(<ConfigsPage />);
  expect(await screen.findByText(/加载配置失败：查询配置类型失败/)).toBeInTheDocument();
});

test("二级详情：显示面包屑和配置项，停用的类型有提示", async () => {
  fetchMock.mockImplementation(async () =>
    Response.json({ type: type({ status: "disabled" }), items: { total: 2, page: 1, page_size: 10, can_write: false, rows: [item(), item({ id: 12, name: "判定员", code: "judge", status: "disabled" })] }, can_write: false }),
  );
  render(<ConfigDetailPage code="role_limit" />);
  expect(await screen.findByText("岗位角色上限（role_limit）")).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "参数配置" })).toHaveAttribute("href", "#/configs");
  expect(screen.getByText("判定员")).toBeInTheDocument();
  expect(screen.getByText(/这个配置类型已停用/)).toBeInTheDocument();
  for (const button of screen.getAllByRole("button", { name: /删\s?除/ })) expect(button).toBeDisabled();
});

test("二级详情：类型不存在时显示服务端原因", async () => {
  fetchMock.mockImplementation(async () => new Response("没有这个配置类型", { status: 404 }));
  render(<ConfigDetailPage code="nope" />);
  expect(await screen.findByText("没有这个配置类型")).toBeInTheDocument();
});

const page = <T,>(rows: T[], canWrite: boolean) => ({ total: rows.length, page: 1, page_size: 10, rows, can_write: canWrite });
const posts = () => fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");

test("管理员：能新增配置类型，请求发到服务端，编码格式不对时不提交", async () => {
  fetchMock.mockImplementation(async (_url, init) => (init?.method === "POST" ? new Response(null, { status: 201 }) : Response.json(page([type()], true))));
  const user = userEvent.setup();
  render(<ConfigsPage />);
  await screen.findByRole("link", { name: "role_limit" });
  await user.click(screen.getByRole("button", { name: /新\s?增/ }));
  await user.type(await within(await screen.findByRole("dialog")).findByLabelText("名称"), "测试类型");
  await user.type(within(screen.getByRole("dialog")).getByLabelText("编码"), "Bad Code");
  await user.click(screen.getByRole("button", { name: /保\s?存/ }));
  expect(await screen.findByText(/编码只能用小写字母/)).toBeInTheDocument();
  expect(posts()).toHaveLength(0);
  await user.clear(within(screen.getByRole("dialog")).getByLabelText("编码"));
  await user.type(within(screen.getByRole("dialog")).getByLabelText("编码"), "demo_type");
  await user.click(screen.getByRole("button", { name: /保\s?存/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(String(posts()[0][0])).toBe("/api/configs");
  expect(JSON.parse(String(posts()[0][1]?.body))).toMatchObject({ name: "测试类型", code: "demo_type", status: "normal" });
});

test("管理员：服务端拒绝（值里有密码）时在抽屉里显示原因", async () => {
  fetchMock.mockImplementation(async (_url, init) => (init?.method === "POST" ? new Response("值里不能出现密码或 API Key", { status: 400 }) : Response.json(page([type()], true))));
  const user = userEvent.setup();
  render(<ConfigsPage />);
  await screen.findByRole("link", { name: "role_limit" });
  await user.click(screen.getByRole("button", { name: /新\s?增/ }));
  await user.type(await within(await screen.findByRole("dialog")).findByLabelText("名称"), "x");
  await user.type(within(screen.getByRole("dialog")).getByLabelText("编码"), "demo_type");
  await user.click(screen.getByRole("button", { name: /保\s?存/ }));
  expect(await screen.findByText("值里不能出现密码或 API Key")).toBeInTheDocument();
});

test("管理员：配置项能禁用，禁用请求带原来的值", async () => {
  fetchMock.mockImplementation(async (_url, init) =>
    init?.method === "POST"
      ? new Response(null, { status: 204 })
      : Response.json({ type: type(), can_write: true, items: page([item()], true) }),
  );
  const user = userEvent.setup();
  render(<ConfigDetailPage code="role_limit" />);
  await user.click(await screen.findByRole("button", { name: /禁\s?用/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(String(posts()[0][0])).toBe("/api/configs/role_limit/items/11/update");
  expect(JSON.parse(String(posts()[0][1]?.body))).toMatchObject({ name: "执行员", value: "3", status: "disabled" });
});

test("不是管理员：写操作按钮都禁用", async () => {
  fetchMock.mockImplementation(async () => Response.json(page([type()], false)));
  render(<ConfigsPage />);
  await screen.findByRole("link", { name: "role_limit" });
  expect(screen.getByRole("button", { name: /新\s?增/ })).toBeDisabled();
  expect(screen.getByRole("button", { name: /修\s?改/ })).toBeDisabled();
});
