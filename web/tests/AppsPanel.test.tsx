import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { AppView, RoleInput, RoleView } from "../src/api";
import AppsPanel from "../src/AppsPanel";

const fetchMock = vi.fn<typeof fetch>();
const NODE = "aaaaaaaa-0000-0000-0000-000000000001";

const scanned = (source: string, over: Partial<AppView>): AppView => ({
  source, scanned: true, installed: true, version: "1.0", login: "ok", login_detail: "已登录", models: [], models_note: "", scanned_at: "2026-11-23T01:00:00Z", ...over,
});

const codex = scanned("codex_exec", {
  login_detail: "已用 ChatGPT 登录",
  models: [
    { id: "gpt-a", display: "GPT-A", efforts: ["low", "high", "max"], default_effort: "low" },
    { id: "gpt-b", display: "GPT-B", efforts: ["low", "medium"], default_effort: "medium" },
  ],
  models_note: "来自 codex debug models，共 2 个可用模型",
});
const agy = scanned("agy", { models: [{ id: "gemini-x-high", display: "Gemini X (High)", efforts: [], default_effort: null }], models_note: "来自 agy models" });
const claude = scanned("claude_cli", { models: [{ id: "opus", display: "Opus（最新）", efforts: [], default_effort: null }], models_note: "Claude 命令行不能列出模型：这里是别名建议，也可以手填完整模型名" });

function serve(apps: AppView[], online = true) {
  const unscanned = (source: string): AppView => ({ source, scanned: false });
  const all = ["claude_cli", "codex_exec", "agy", "deepseek"].map((s) => apps.find((a) => a.source === s) ?? unscanned(s));
  // 真实节点每次检查都会写新的 scanned_at；面板靠它判断「等到新结果了」，所以收到检查请求后换一个时间
  let rescans = 0;
  fetchMock.mockImplementation(async (input, init) => {
    const path = String(input);
    if (path === `/api/nodes/${NODE}/apps`) return Response.json({ online, apps: all.map((a) => (a.scanned && rescans > 0 ? { ...a, scanned_at: `${a.scanned_at}#${rescans}` } : a)) });
    if (init?.method === "POST" && path.includes("/apps/")) {
      rescans += 1;
      return new Response(null, { status: 202 });
    }
    return new Response("没有这个接口", { status: 404 });
  });
}

const existing: RoleView[] = [];

function mount(onCreate: (roles: RoleInput[]) => void = () => {}, roles: RoleView[] = existing, busy = false) {
  return render(<AppsPanel nodeId={NODE} userToken="u_synthetic" online existingRoles={roles} busy={busy} onCreateRoles={onCreate} />);
}

const scanCalls = () => fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});
afterEach(() => {
  vi.unstubAllGlobals();
  fetchMock.mockReset();
});

test("四个应用卡片都显示，状态有文字；DeepSeek 显示开发中且点不进去", async () => {
  serve([codex, scanned("agy", { installed: false, login_detail: "本机没有找到 agy 命令行" }), scanned("claude_cli", { login: "not_logged_in", login_detail: "未登录" })]);
  const user = userEvent.setup();
  mount();
  for (const name of ["Claude", "Codex", "Gemini", "DeepSeek"]) expect(await screen.findByText(name)).toBeInTheDocument();
  expect(await screen.findByText("检测到并可用")).toBeInTheDocument();
  expect(screen.getByText("未安装")).toBeInTheDocument();
  expect(screen.getByText("未登录")).toBeInTheDocument();
  expect(screen.getAllByText("开发中").length).toBeGreaterThan(0);
  await user.click(screen.getByText("DeepSeek").closest("button")!);
  expect(scanCalls()).toHaveLength(0);
  expect(screen.queryByText(/检查本机环境/)).not.toBeInTheDocument();
});

test("点应用自动检查本机（不要审核口令），结果显示三行检查清单", async () => {
  serve([codex]);
  const user = userEvent.setup();
  mount();
  await screen.findByText("检测到并可用");
  await user.click(screen.getByText("Codex").closest("button")!);
  await waitFor(() => expect(scanCalls()).toHaveLength(1));
  const [url, init] = scanCalls()[0];
  expect(String(url)).toBe(`/api/nodes/${NODE}/apps/codex_exec/scan`);
  expect((init?.headers as Record<string, string>)["x-user-token"]).toBe("u_synthetic");
  expect(await screen.findByText(/已用 ChatGPT 登录/)).toBeInTheDocument();
  expect(screen.getByText(/来自 codex debug models/)).toBeInTheDocument();
  expect(screen.getAllByText("通过").length).toBe(3);
  expect(screen.getByText(/不调用模型、不花额度/)).toBeInTheDocument();
});

test("没装或登录方式不对：不能继续创建角色，并给出原因", async () => {
  serve([scanned("agy", { installed: false, login_detail: "本机没有找到 agy 命令行" }), scanned("claude_cli", { login: "wrong_auth", login_detail: "不是会员登录" })]);
  const user = userEvent.setup();
  mount();
  await screen.findAllByText("未安装");
  await user.click(screen.getByText("Gemini").closest("button")!);
  expect(await screen.findByText(/本机没有找到 Gemini，不能继续/)).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /创建角色/ })).not.toBeInTheDocument();
  await user.click(screen.getByText("Claude").closest("button")!);
  expect(await screen.findByText(/登录方式不对，不能继续/)).toBeInTheDocument();
  expect(screen.getAllByText(/不使用 API Key/).length).toBeGreaterThan(0);
});

test("选模型创建执行员角色：只含勾选的模型，默认强度取该模型的默认档位", async () => {
  serve([codex]);
  const onCreate = vi.fn<(roles: RoleInput[]) => void>();
  const user = userEvent.setup();
  mount(onCreate);
  await screen.findByText("检测到并可用");
  await user.click(screen.getByText("Codex").closest("button")!);
  await screen.findByText("选模型并创建角色");
  // 没勾选就创建：中文提示，不调用
  await user.click(screen.getByRole("button", { name: /创建角色/ }));
  expect(await screen.findByText("请至少勾选一个模型")).toBeInTheDocument();
  expect(onCreate).not.toHaveBeenCalled();
  // 只勾 GPT-B：默认模型自动选它，强度取它自己的默认档位 medium
  await user.click(screen.getByRole("checkbox", { name: /GPT-B/ }));
  await user.click(screen.getByRole("button", { name: /创建角色/ }));
  expect(onCreate).toHaveBeenCalledTimes(1);
  expect(onCreate.mock.calls[0][0]).toEqual([
    { id: "codex_exec-executor", slot: "executor", name: "Codex 执行员", source: "codex_exec", default: { model: "gpt-b", effort: "medium" }, models: [{ model: "gpt-b", efforts: ["low", "medium"] }] },
  ]);
});

test("岗位按来源限制；agy 的模型没有强度；角色 id 和现有角色不重复", async () => {
  serve([agy, claude]);
  const onCreate = vi.fn<(roles: RoleInput[]) => void>();
  const user = userEvent.setup();
  const taken = [{ id: "agy-judge", slot: "judge", name: "x", source: "agy", default: { model: "m", effort: null }, models: [], key_configured: false, check: { status: "ok", detail: null, checked_at: null } }] as RoleView[];
  mount(onCreate, taken);
  await screen.findAllByText("检测到并可用");
  await user.click(screen.getByText("Gemini").closest("button")!);
  await screen.findByText("选模型并创建角色");
  // agy 只能当判定员：岗位选项里没有执行员
  const slots = screen.getByText("岗位").parentElement!;
  expect(within(slots).queryByText("执行员")).not.toBeInTheDocument();
  expect(within(slots).getByText("判定员")).toBeInTheDocument();
  expect(screen.getByText("强度已包含在模型名里")).toBeInTheDocument();
  await user.click(screen.getByRole("checkbox", { name: /Gemini X/ }));
  await user.click(screen.getByRole("button", { name: /创建角色/ }));
  const role = onCreate.mock.calls[0][0][0];
  expect(role.id).toBe("agy-judge-2");
  expect(role.default).toEqual({ model: "gemini-x-high", effort: null });
});

test("Claude 没有模型列表：说明原因，允许手动添加模型名", async () => {
  serve([claude]);
  const onCreate = vi.fn<(roles: RoleInput[]) => void>();
  const user = userEvent.setup();
  mount(onCreate);
  await screen.findByText("检测到并可用");
  await user.click(screen.getByText("Claude").closest("button")!);
  expect(await screen.findByText(/Claude 命令行不能列出模型/)).toBeInTheDocument();
  await user.type(screen.getByLabelText("手动添加模型名"), "my-full-model-name{Enter}");
  expect(screen.getByRole("checkbox", { name: /my-full-model-name/ })).toBeChecked();
  await user.click(screen.getByRole("button", { name: /创建角色/ }));
  const role = onCreate.mock.calls[0][0][0];
  expect(role.models).toEqual([{ model: "my-full-model-name", efforts: [] }]);
  expect(role.default.model).toBe("my-full-model-name");
});

test("父页面正在保存时创建按钮禁用", async () => {
  serve([codex]);
  const user = userEvent.setup();
  mount(() => {}, existing, true);
  await screen.findByText("检测到并可用");
  await user.click(screen.getByText("Codex").closest("button")!);
  await screen.findByText("选模型并创建角色");
  expect(screen.getByRole("button", { name: /正在保存/ })).toBeDisabled();
});
