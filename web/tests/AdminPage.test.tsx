import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { AdminChat, AdminMessage, AdminProposal } from "../src/api";
import AdminPage from "../src/AdminPage";
import { rememberUserName } from "../src/identity";
import { rememberReviewToken } from "../src/reviewLogic";

const fetchMock = vi.fn<typeof fetch>();
const REVIEW = "synthetic-review-token-012345";

function proposal(over: Partial<AdminProposal> & Pick<AdminProposal, "id" | "kind">): AdminProposal {
  return { args: {}, why: "", status: "pending", result: "", ...over };
}
function message(over: Partial<AdminMessage> & Pick<AdminMessage, "id" | "author">): AdminMessage {
  return { text: "", is_error: false, created_at: "2026-11-23T10:00:00Z", proposals: [], ...over };
}

let chat: AdminChat = { pending: null, messages: [] };
const posts = () => fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");
const header = (init: RequestInit | undefined, name: string) => (init?.headers as Record<string, string>)[name];

function serve(next: AdminChat, reply: (path: string) => Response = () => new Response("{}", { status: 202 })) {
  chat = next;
  fetchMock.mockImplementation(async (input, init) => {
    const path = String(input);
    if (path === "/api/admin/chat") return Response.json(chat);
    if (path === "/api/projects/hafa/modules") return Response.json([]);
    if (path === "/api/nodes") return Response.json([]);
    if (init?.method === "POST") return reply(path);
    return new Response("没有这个接口", { status: 404 });
  });
}

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  rememberUserName("小明");
  rememberReviewToken(REVIEW);
});
afterEach(() => {
  vi.unstubAllGlobals();
  fetchMock.mockReset();
  rememberReviewToken("");
});

test("没填名字：提示先填「我是谁」，输入框和发送都用不了，也不读对话", async () => {
  window.localStorage.clear();
  serve({ pending: null, messages: [] });
  render(<AdminPage />);
  expect(await screen.findByText("先填「我是谁」")).toBeInTheDocument();
  expect(screen.getByLabelText("对管理员说")).toBeDisabled();
  expect(screen.getByRole("button", { name: /发\s*送/ })).toBeDisabled();
  expect(fetchMock.mock.calls.some(([input]) => String(input) === "/api/admin/chat")).toBe(false);
});

test("提议卡片：状态有文字，待确认的有确认和忽略，失败原因原样显示，管理员出错标成错误", async () => {
  serve({
    pending: null,
    messages: [
      message({ id: "m1", author: "user", text: "新增充值需求" }),
      message({
        id: "m2", author: "admin", text: "建议先新建模块",
        proposals: [
          proposal({ id: "p1", kind: "create_module", args: { name: "充值" }, why: "文档新增了充值" }),
          proposal({ id: "p2", kind: "run_tests", args: { module: "语聊房", case_codes: ["GEN-1", "GEN-2"] }, status: "failed", result: "你的节点不在线，没法发起测试" }),
          proposal({ id: "p3", kind: "generate_cases", args: { module: "个人主页" }, status: "dismissed" }),
          proposal({ id: "p4", kind: "run_tests", args: { module: "语聊房", case_codes: ["GEN-3"] }, status: "confirmed", result: "已用你的节点发起测试" }),
        ],
      }),
      message({ id: "m3", author: "admin", text: "管理员没有回答成功：Claude 会员额度用完或被限流", is_error: true }),
    ],
  });
  render(<AdminPage />);
  expect(await screen.findByText("新建模块「充值」")).toBeInTheDocument();
  expect(screen.getByText(/理由：文档新增了充值/)).toBeInTheDocument();
  for (const text of ["待你确认", "执行失败", "已忽略", "已确认"]) expect(screen.getAllByText(text).length, text).toBeGreaterThan(0);
  expect(screen.getByText(/失败原因：你的节点不在线/)).toBeInTheDocument();
  expect(screen.getByText(/对模块「语聊房」发起测试（2 条已批准的用例）/)).toBeInTheDocument();
  // 只有待确认的那一张有按钮
  expect(screen.getAllByRole("button", { name: /^确\s*认$/ })).toHaveLength(1);
  expect(screen.getAllByRole("button", { name: /^忽\s*略$/ })).toHaveLength(1);
  expect(screen.getByText(/管理员（出错了）/)).toBeInTheDocument();
  expect(screen.getByText(/额度用完/)).toBeInTheDocument();
});

test("发送：带着「我是谁」和审核口令，发出去之后清空输入框；管理员还在回答时不能再发", async () => {
  serve({ pending: null, messages: [] });
  const user = userEvent.setup();
  render(<AdminPage />);
  await user.type(await screen.findByLabelText("对管理员说"), "语聊房现在能测吗？");
  await user.click(screen.getByRole("button", { name: /发\s*送/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  const [url, init] = posts()[0];
  expect(String(url)).toBe("/api/admin/messages");
  expect(JSON.parse(String(init?.body))).toEqual({ message: "语聊房现在能测吗？" });
  expect(header(init, "x-user-name")).toBe(encodeURIComponent("小明"));
  expect(header(init, "x-review-token")).toBe(REVIEW);
  await waitFor(() => expect(screen.getByLabelText("对管理员说")).toHaveValue(""));
});

test("管理员正在回答：提示它在想，输入框不能用", async () => {
  serve({ pending: { ask_id: "a1", since: "2026-11-23T10:00:00Z" }, messages: [message({ id: "m1", author: "user", text: "你好" })] });
  render(<AdminPage />);
  expect(await screen.findByText(/管理员正在读现状/)).toBeInTheDocument();
  expect(screen.getByLabelText("对管理员说")).toBeDisabled();
});

test("确认发起测试：POST 确认（带名字和审核口令），显示执行结果", async () => {
  serve(
    { pending: null, messages: [message({ id: "m1", author: "admin", text: "可以测", proposals: [proposal({ id: "p1", kind: "run_tests", args: { module: "语聊房", case_codes: ["GEN-1"] } })] })] },
    () => Response.json({ result: "已用你的节点发起测试：1 条用例，任务 1a2b3c4d", next: { task_id: "t1" } }),
  );
  const user = userEvent.setup();
  render(<AdminPage />);
  await user.click(await screen.findByRole("button", { name: /^确\s*认$/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(String(posts()[0][0])).toBe("/api/admin/proposals/p1/confirm");
  expect(header(posts()[0][1], "x-review-token")).toBe(REVIEW);
  expect(header(posts()[0][1], "x-user-name")).toBe(encodeURIComponent("小明"));
  expect(await screen.findByText(/已用你的节点发起测试/)).toBeInTheDocument();
});

test("确认失败：原样显示服务端的原因", async () => {
  serve(
    { pending: null, messages: [message({ id: "m1", author: "admin", text: "可以测", proposals: [proposal({ id: "p1", kind: "run_tests", args: { module: "语聊房", case_codes: ["GEN-1"] } })] })] },
    () => new Response("提议已经过期，没有执行：发起测试：模块「语聊房」里没有提到的已批准用例", { status: 409 }),
  );
  const user = userEvent.setup();
  render(<AdminPage />);
  await user.click(await screen.findByRole("button", { name: /^确\s*认$/ }));
  expect(await screen.findByText(/提议已经过期，没有执行/)).toBeInTheDocument();
});

test("确认「生成用例」：管理员拿不到截图，所以打开预填好模块名的表单，由人补上再提交", async () => {
  serve(
    { pending: null, messages: [message({ id: "m1", author: "admin", text: "充值模块需要生成用例", proposals: [proposal({ id: "p1", kind: "generate_cases", args: { module: "充值" } })] })] },
    () => Response.json({ result: "请在表单里补上「充值」的 UI 链接和区域截图后提交", next: { open_generate: { module: "充值" } } }),
  );
  const user = userEvent.setup();
  render(<AdminPage />);
  await user.click(await screen.findByRole("button", { name: /^确\s*认$/ }));
  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByText("新建任务（生成用例）")).toBeInTheDocument();
  expect(within(dialog).getByDisplayValue("充值")).toBeInTheDocument();
  // 表单里的必填项还是空的：管理员没替人填链接和截图
  expect(within(dialog).queryByDisplayValue(/https?:\/\//)).not.toBeInTheDocument();
});

test("忽略：POST 忽略，不要审核口令", async () => {
  rememberReviewToken("");
  serve({ pending: null, messages: [message({ id: "m1", author: "admin", text: "建议", proposals: [proposal({ id: "p1", kind: "create_module", args: { name: "充值" } })] })] }, () => new Response(null, { status: 204 }));
  const user = userEvent.setup();
  render(<AdminPage />);
  await user.click(await screen.findByRole("button", { name: /^忽\s*略$/ }));
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(String(posts()[0][0])).toBe("/api/admin/proposals/p1/dismiss");
  expect(screen.queryByLabelText("审核口令")).not.toBeInTheDocument();
});

test("没有审核口令时发送先弹出口令框，不直接发请求", async () => {
  rememberReviewToken("");
  serve({ pending: null, messages: [] });
  const user = userEvent.setup();
  render(<AdminPage />);
  await user.type(await screen.findByLabelText("对管理员说"), "你好");
  await user.click(screen.getByRole("button", { name: /发\s*送/ }));
  expect(await screen.findByLabelText("审核口令")).toBeInTheDocument();
  expect(posts()).toHaveLength(0);
});
