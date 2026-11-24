import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { AppView, RoleView } from "../src/api";
import AppsPanel, { type AppRoleDraft } from "../src/AppsPanel";

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

function mount(onDraft: (draft: AppRoleDraft) => void = () => {}, roles: RoleView[] = existing, busy = false, readOnlyReason?: string) {
  return render(<AppsPanel nodeId={NODE} userName="小明" online existingRoles={roles} busy={busy} onDraftRole={onDraft} readOnlyReason={readOnlyReason} />);
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

test("点应用自动检查本机（不要审核口令，带「我是谁」），结果显示三行检查清单", async () => {
  serve([codex]);
  const user = userEvent.setup();
  mount();
  await screen.findByText("检测到并可用");
  await user.click(screen.getByText("Codex").closest("button")!);
  await waitFor(() => expect(scanCalls()).toHaveLength(1));
  const [url, init] = scanCalls()[0];
  expect(String(url)).toBe(`/api/nodes/${NODE}/apps/codex_exec/scan`);
  expect((init?.headers as Record<string, string>)["x-user-name"]).toBe(encodeURIComponent("小明"));
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
  expect(screen.queryByRole("button", { name: /新增角色/ })).not.toBeInTheDocument();
  await user.click(screen.getByText("Claude").closest("button")!);
  expect(await screen.findByText(/登录方式不对，不能继续/)).toBeInTheDocument();
  expect(screen.getAllByText(/不使用 API Key/).length).toBeGreaterThan(0);
});

test("环境检查通过后，点按钮把来源和全部检查到的模型带去「新增角色」编辑区", async () => {
  serve([codex]);
  const onDraft = vi.fn<(draft: AppRoleDraft) => void>();
  const user = userEvent.setup();
  mount(onDraft);
  await screen.findByText("检测到并可用");
  await user.click(screen.getByText("Codex").closest("button")!);
  // 面板里不再有第 3 步的表单
  expect(await screen.findByRole("button", { name: /用这些模型新增角色/ })).toBeInTheDocument();
  expect(screen.queryByText("选模型并创建角色")).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: /用这些模型新增角色/ }));
  expect(onDraft).toHaveBeenCalledTimes(1);
  expect(onDraft.mock.calls[0][0]).toEqual({
    source: "codex_exec",
    slot: "executor",
    name: "研发流程测试",
    models: [{ model: "gpt-a", efforts: ["low", "high", "max"] }, { model: "gpt-b", efforts: ["low", "medium"] }],
    default: { model: "gpt-a", effort: "low" },
  });
});

test("岗位按来源给默认值：Gemini、Claude 默认判定员；没有强度的模型 effort 为空", async () => {
  serve([agy, claude]);
  const onDraft = vi.fn<(draft: AppRoleDraft) => void>();
  const user = userEvent.setup();
  mount(onDraft);
  await screen.findAllByText("检测到并可用");
  await user.click(screen.getByText("Gemini").closest("button")!);
  await user.click(await screen.findByRole("button", { name: /用这些模型新增角色/ }));
  expect(onDraft.mock.calls[0][0]).toMatchObject({ source: "agy", slot: "judge", default: { model: "gemini-x-high", effort: null } });
  await user.click(screen.getByText("Claude").closest("button")!);
  await user.click(await screen.findByRole("button", { name: /用这些模型新增角色/ }));
  expect(onDraft.mock.calls[1][0]).toMatchObject({ source: "claude_cli", slot: "judge" });
});

test("父页面正在保存时按钮禁用", async () => {
  serve([codex]);
  const user = userEvent.setup();
  mount(() => {}, existing, true);
  await screen.findByText("检测到并可用");
  await user.click(screen.getByText("Codex").closest("button")!);
  expect(await screen.findByRole("button", { name: /用这些模型新增角色/ })).toBeDisabled();
});

test("只读（不是节点主人或没填名字）：能看到上次的检查结果，但不触发检查、不能创建角色", async () => {
  serve([codex]);
  const user = userEvent.setup();
  mount(() => {}, existing, false, "这是别人的节点：你能看，但只能修改和使用自己的节点");
  expect(await screen.findByText(/这是别人的节点/)).toBeInTheDocument();
  expect(await screen.findByText("检测到并可用")).toBeInTheDocument();
  await user.click(screen.getByText("Codex").closest("button")!);
  // 看得到上一次的结果，但不发检查请求，也没有「重新检查」「创建角色」
  expect(await screen.findByText(/已用 ChatGPT 登录/)).toBeInTheDocument();
  expect(scanCalls()).toHaveLength(0);
  expect(screen.queryByRole("button", { name: /重新检查/ })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /新增角色/ })).not.toBeInTheDocument();
  expect(screen.getByText(/只有节点主人能用这些模型创建角色/)).toBeInTheDocument();
});

test("只读且这个节点还没检查过：说明只有节点主人能触发检查", async () => {
  serve([]);
  const user = userEvent.setup();
  mount(() => {}, existing, false, "你现在是只读");
  await user.click((await screen.findByText("Codex")).closest("button")!);
  expect(await screen.findByText(/只有节点主人能触发检查/)).toBeInTheDocument();
  expect(scanCalls()).toHaveLength(0);
});
