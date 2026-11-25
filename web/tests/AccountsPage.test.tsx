import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import AccountsPage from "../src/AccountsPage";
import type { PoolAccount } from "../src/api";

const fetchMock = vi.fn<typeof fetch>();

const account = (over: Partial<PoolAccount> = {}): PoolAccount => ({
  id: "11111111-1111-1111-1111-111111111111", project: "hafa", env: "debug", nickname: "合成昵称", status: "idle", device_serial: null, identity: null, ...over,
});

/** 身份选项只来自参数配置「账号身份」这个类型下的配置项 */
const identityConfig = (canWrite: boolean, items: { code: string; name: string; status?: string }[]) => ({
  type: { id: 9, name: "账号身份", code: "account_identity", remark: "", status: "normal", item_count: items.length, updated_at: "2026-11-24T00:00:00Z", updated_by: null },
  items: {
    total: items.length, page: 1, page_size: 100, can_write: canWrite,
    rows: items.map((i, n) => ({ id: n + 1, name: i.name, code: i.code, value: i.code, label: "", group_name: "", sort: n, remark: "", status: i.status ?? "normal", updated_at: "2026-11-24T00:00:00Z", updated_by: null })),
  },
  can_write: canWrite,
});

function route(canWrite: boolean, items: { code: string; name: string; status?: string }[], accounts: PoolAccount[]) {
  fetchMock.mockImplementation(async (input) => {
    const url = String(input);
    if (url.includes("/api/account-pool") && !url.includes("/identity")) return Response.json(accounts);
    if (url.includes("/api/configs/account_identity")) return Response.json(identityConfig(canWrite, items));
    return new Response("{}", { status: 200 });
  });
}

beforeEach(() => vi.stubGlobal("fetch", fetchMock));
afterEach(() => { vi.unstubAllGlobals(); fetchMock.mockReset(); });

test("账号列表只显示元数据，不显示用户名密码；非管理员不能改身份", async () => {
  route(false, [{ code: "vip", name: "VIP 用户" }], [account({ identity: "vip" })]);
  render(<AccountsPage />);
  expect(await screen.findByText("合成昵称")).toBeInTheDocument();
  expect(screen.queryByText(/password|密码：/i)).not.toBeInTheDocument();
  expect(screen.getByText("VIP 用户（vip）")).toBeInTheDocument();
  // antd Select 禁用时根节点带 disabled 样式
  expect(document.querySelector(".ant-select-disabled")).not.toBeNull();
});

test("身份选项全部来自配置：配置里没有就没有选项，停用的身份不出现", async () => {
  route(true, [{ code: "host", name: "主播" }, { code: "old", name: "旧身份", status: "disabled" }], [account()]);
  const user = userEvent.setup();
  render(<AccountsPage />);
  await screen.findByText("合成昵称");
  await user.click(screen.getByRole("combobox", { name: /的身份/ }));
  expect(await screen.findByText("主播（host）")).toBeInTheDocument();
  expect(screen.queryByText(/旧身份/)).not.toBeInTheDocument();
  expect(screen.queryByText(/VIP|vip/)).not.toBeInTheDocument();
});

test("管理员勾选身份：把编码提交给接口", async () => {
  route(true, [{ code: "host", name: "主播" }], [account()]);
  const user = userEvent.setup();
  render(<AccountsPage />);
  await screen.findByText("合成昵称");
  await user.click(screen.getByRole("combobox", { name: /的身份/ }));
  await user.click(await screen.findByText("主播（host）"));
  await waitFor(() => {
    const call = fetchMock.mock.calls.find(([url]) => String(url).includes("/identity"));
    expect(call).toBeTruthy();
    expect(JSON.parse(String(call![1]?.body))).toEqual({ identity: "host" });
  });
});

test("账号上的身份已被配置删除时仍能看到，不会悄悄变成不区分", async () => {
  route(true, [], [account({ identity: "gone" })]);
  render(<AccountsPage />);
  expect(await screen.findByText("gone（已不在配置里）")).toBeInTheDocument();
});
