import { render, screen, waitFor } from "@testing-library/react";
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
  fetchMock.mockImplementation(async () => Response.json({ total: 1, page: 1, page_size: 10, rows: [type()] }));
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
    Response.json({ type: type({ status: "disabled" }), items: { total: 2, page: 1, page_size: 10, rows: [item(), item({ id: 12, name: "判定员", code: "judge", status: "disabled" })] } }),
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
