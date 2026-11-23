import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { ModuleCase, ModuleView, NodeView, ReviewStatus } from "../src/api";
import GenerateDialog from "../src/GenerateDialog";
import ModuleDetailPage from "../src/ModuleDetailPage";
import ModulesPage from "../src/ModulesPage";
import { rememberReviewToken } from "../src/reviewLogic";

const fetchMock = vi.fn<typeof fetch>();
const NODE = "aaaaaaaa-0000-0000-0000-000000000001";
const MOD = "bbbbbbbb-0000-0000-0000-000000000001";
const nodes: NodeView[] = [{ node_id: NODE, online: true, caps: { node_version: "t", os: "macos", model_sources: [] }, devices: [], last_seen: "2026-11-23T00:00:00Z" }];

function mod(overrides: Partial<ModuleView> = {}): ModuleView {
  return { id: MOD, name: "语聊房", created_at: "2026-11-23T00:00:00Z", case_count: 3, approved_count: 0, needs_human_count: 1, active_runs: 0, latest_request: { id: "r", status: "finished", detail: "生成了 3 条用例（草稿，待复核）", requested_at: "2026-11-23T00:00:00Z" }, ...overrides };
}

function kase(code: string, status: ReviewStatus, note = "", extra: Partial<ModuleCase> = {}): ModuleCase {
  return { code, title: `标题-${code}`, origin: "generated", review_status: status, review_note: note, steps: ["点击入口"], expected: ["出现页面"], latest_run: null, ...extra };
}

function serve(module: ModuleView, cases: ModuleCase[]) {
  fetchMock.mockImplementation(async (input, init) => {
    const path = String(input);
    if (init?.method === "POST") return new Response(JSON.stringify({ approved: 1, request_id: "r", module_id: MOD, doc_id: "d" }), { status: path.endsWith("/casegen-requests") ? 201 : 200 });
    if (path === "/api/nodes") return Response.json(nodes);
    if (path === "/api/projects/hafa/modules") return Response.json([module]);
    if (path === `/api/modules/${MOD}/cases`) return Response.json(cases);
    if (path === "/api/projects/hafa/cases") return Response.json([]);
    return new Response("没有这个接口", { status: 404 });
  });
}

const posts = () => fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  rememberReviewToken("");
});
afterEach(() => {
  vi.unstubAllGlobals();
  fetchMock.mockReset();
});

test("创建任务：必填项缺哪项提示哪项，缺项时不发请求", async () => {
  serve(mod(), []);
  const user = userEvent.setup();
  render(<GenerateDialog onClose={() => {}} />);
  await user.click(await screen.findByRole("button", { name: /提交生成任务/ }));
  for (const message of [/请填写或上传需求文档/, /请至少填写 1 个 UI 图链接/, /请至少上传 1 张 UI 区域截图/, /请填「我是谁」/]) {
    expect((await screen.findAllByText(message)).length, String(message)).toBeGreaterThan(0);
  }
  expect(posts()).toHaveLength(0);
  // 链接不会被打开、文档和截图会外发，都要明确告诉用户
  expect(screen.getAllByText(/AI 不会打开它/).length).toBeGreaterThan(0);
  expect(screen.getAllByText(/发给你所选角色的模型来源/).length).toBeGreaterThan(0);
});

test("模块列表：生成失败原样显示原因，阶段用文字", async () => {
  serve(mod({ case_count: 0, needs_human_count: 0, latest_request: { id: "r", status: "failed", detail: "生成的用例没有通过校验，整批没有入库：第 2 条用例：steps 不能为空", requested_at: "2026-11-23T00:00:00Z" } }), []);
  render(<ModulesPage />);
  expect(await screen.findByText("生成失败")).toBeInTheDocument();
  expect(screen.getByText(/整批没有入库：第 2 条用例：steps 不能为空/)).toBeInTheDocument();
});

test("复核：评审员认为没问题只是意见，不显示成通过也不显示已批准", async () => {
  serve(mod(), [kase("GEN-1", "reviewed_ok", "文档能对上"), kase("GEN-2", "needs_human", "文档没写昵称长度"), kase("GEN-3", "approved")]);
  render(<ModuleDetailPage id={MOD} />);
  await screen.findAllByText("标题-GEN-1");
  const row = (code: string) => screen.getAllByText(`标题-${code}`)[0].closest("div[class*='mod-']")!.parentElement as HTMLElement;
  expect(within(row("GEN-1")).getByText(/评审员认为没问题（待你确认）/)).toBeInTheDocument();
  expect(within(row("GEN-1")).queryByText("已批准")).not.toBeInTheDocument();
  expect(within(row("GEN-1")).queryByText(/^通过$/)).not.toBeInTheDocument();
  expect(within(row("GEN-2")).getByText(/需要人工确认/)).toBeInTheDocument();
  expect(screen.getAllByText("文档没写昵称长度").length).toBeGreaterThan(0);
  // 没在测试中：普通布局，没有指挥台
  expect(screen.queryByText("测试指挥台")).not.toBeInTheDocument();
});

test("复核：批准评审员有疑问的用例必须写备注，没写不发请求", async () => {
  rememberReviewToken("synthetic-review-token-012345");
  serve(mod(), [kase("GEN-2", "needs_human", "文档没写昵称长度")]);
  const user = userEvent.setup();
  render(<ModuleDetailPage id={MOD} />);
  await screen.findAllByText("标题-GEN-2");
  await user.type(screen.getByLabelText("我是谁"), "小明");
  await user.click((await screen.findAllByRole("button", { name: /填备注并批准/ }))[0]);
  await user.click(await screen.findByRole("button", { name: /确\s*认\s*批\s*准/ }));
  expect((await screen.findAllByText(/必须填写备注/)).length).toBeGreaterThan(0);
  expect(posts()).toHaveLength(0);
});

test("测试中的模块切到指挥台布局，复核功能仍在", async () => {
  serve(mod({ active_runs: 1, approved_count: 2, case_count: 2, needs_human_count: 0 }), [
    kase("GEN-1", "approved", "", { latest_run: { run_id: "r1", task_id: "t1", status: "running", final_result: null, blocked_reason: null, queued_at: "2026-11-23T00:00:00Z" } }),
    kase("GEN-2", "approved", "", { latest_run: { run_id: "r2", task_id: "t1", status: "finished", final_result: "failed", blocked_reason: null, queued_at: "2026-11-23T00:00:00Z" } }),
  ]);
  render(<ModuleDetailPage id={MOD} />);
  expect(await screen.findByText("测试指挥台")).toBeInTheDocument();
  // 失败要有文字，且不是「通过」
  expect(screen.getAllByText(/失败/).length).toBeGreaterThan(0);
  await waitFor(() => expect(screen.getByLabelText("用例复核折叠区")).toBeInTheDocument());
});
