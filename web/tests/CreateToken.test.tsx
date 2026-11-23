import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import UserTokenHint from "../src/UserTokenHint";
import { rememberReviewToken } from "../src/reviewLogic";
import { useRememberedUserToken } from "../src/userToken";

const fetchMock = vi.fn<typeof fetch>();
const NEW_TOKEN = `u_${"k7Q2".repeat(16)}`;

function Host() {
  const { token, setToken, expiresAt, forget } = useRememberedUserToken();
  return (
    <div>
      <output aria-label="当前令牌是否已填">{token ? "已填" : "未填"}</output>
      <UserTokenHint expiresAt={expiresAt} onForget={forget} onToken={setToken} />
    </div>
  );
}

const posts = () => fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  rememberReviewToken("synthetic-review-token-012345");
  Object.defineProperty(navigator, "clipboard", { value: { writeText: vi.fn().mockResolvedValue(undefined) }, configurable: true });
});
afterEach(() => {
  vi.unstubAllGlobals();
  fetchMock.mockReset();
  rememberReviewToken("");
});

test("创建个人令牌：令牌只显示这一次，点使用后记住 30 天，关掉就再也看不到", async () => {
  fetchMock.mockResolvedValue(new Response(JSON.stringify({ name: "小明", token: NEW_TOKEN }), { status: 201 }));
  const user = userEvent.setup();
  render(<Host />);
  expect(screen.getByLabelText("当前令牌是否已填")).toHaveTextContent("未填");
  await user.click(screen.getByRole("button", { name: "创建个人令牌" }));
  // 名字必填
  await user.click(screen.getByRole("button", { name: /创\s*建$/ }));
  expect(await screen.findByText(/请填一个名字/)).toBeInTheDocument();
  expect(posts()).toHaveLength(0);
  await user.type(screen.getByLabelText("名字"), "小明");
  await user.click(screen.getByRole("button", { name: /创\s*建$/ }));
  // 只显示这一次，并说明丢了只能重建、节点要设 NODE_USER_TOKEN
  expect(await screen.findByDisplayValue(NEW_TOKEN)).toBeInTheDocument();
  expect(screen.getByText("令牌只显示这一次")).toBeInTheDocument();
  expect(screen.getByText(/NODE_USER_TOKEN/)).toBeInTheDocument();
  expect(String(posts()[0][0])).toBe("/api/users");
  expect((posts()[0][1]?.headers as Record<string, string>)["x-review-token"]).toBe("synthetic-review-token-012345");
  await user.click(screen.getByRole("button", { name: "复制令牌" }));
  expect(await screen.findByText("已复制")).toBeInTheDocument();
  // 使用：填进页面并记住 30 天；之后页面上看不到令牌本身
  await user.click(screen.getByRole("button", { name: "使用这个令牌" }));
  expect(screen.getByLabelText("当前令牌是否已填")).toHaveTextContent("已填");
  expect(await screen.findByText(/已记住，到期/)).toBeInTheDocument();
  expect(screen.queryByDisplayValue(NEW_TOKEN)).not.toBeInTheDocument();
  expect(document.body.textContent).not.toContain(NEW_TOKEN);
});

test("名字重复等失败原样显示服务端的原因，不显示令牌", async () => {
  fetchMock.mockResolvedValue(new Response("这个名字已经有人用了，换一个", { status: 409 }));
  const user = userEvent.setup();
  render(<Host />);
  await user.click(screen.getByRole("button", { name: "创建个人令牌" }));
  await user.type(screen.getByLabelText("名字"), "小明");
  await user.click(screen.getByRole("button", { name: /创\s*建$/ }));
  expect(await screen.findByText(/这个名字已经有人用了/)).toBeInTheDocument();
  await waitFor(() => expect(screen.queryByText("令牌只显示这一次")).not.toBeInTheDocument());
  expect(screen.getByLabelText("当前令牌是否已填")).toHaveTextContent("未填");
});

test("没有审核口令时先弹出输入审核口令，不直接发请求", async () => {
  rememberReviewToken("");
  const user = userEvent.setup();
  render(<Host />);
  await user.click(screen.getByRole("button", { name: "创建个人令牌" }));
  await user.type(screen.getByLabelText("名字"), "小明");
  await user.click(screen.getByRole("button", { name: /创\s*建$/ }));
  expect(await screen.findByLabelText("审核口令")).toBeInTheDocument();
  expect(posts()).toHaveLength(0);
});
