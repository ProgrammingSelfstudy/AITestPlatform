import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { ModuleCase, ModuleShot } from "../src/api";
import ModuleCases from "../src/ModuleCases";
import ModuleResults from "../src/ModuleResults";
import ModuleShots from "../src/ModuleShots";
import { rememberReviewToken } from "../src/reviewLogic";

const fetchMock = vi.fn<typeof fetch>();
const MOD = "bbbbbbbb-0000-0000-0000-000000000001";

const shot = (id: string, name: string): ModuleShot => ({ id, name, content_type: "image/png", size: 2048, created_at: "2026-11-24T00:00:00Z", created_by: "小明" });
const kase = (code: string, over: Partial<ModuleCase> = {}): ModuleCase => ({
  code, title: `标题-${code}`, run_env: "app", level: "高", preconditions: ["已登录"], origin: "imported", review_status: "approved", review_note: "",
  steps: ["点入口", "点充值"], expected: ["到账", "出现流水"], latest_run: null, ...over,
});
const posts = () => fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  rememberReviewToken("tok-1234");
});
afterEach(() => { vi.unstubAllGlobals(); fetchMock.mockReset(); rememberReviewToken(""); });

test("UI 图：显示张数和每张图，上传带名字和口令，能删除", async () => {
  let list = [shot("s1", "首页"), shot("s2", "充值页")];
  fetchMock.mockImplementation(async (input, init) => {
    const path = String(input);
    if (init?.method === "POST" && path.endsWith("/screenshots")) {
      list = [...list, shot("s3", "新图")];
      return Response.json({ added: 1, duplicates: 0 });
    }
    if (init?.method === "POST" && path.endsWith("/delete")) {
      list = list.filter((s) => !path.includes(s.id));
      return new Response(null, { status: 204 });
    }
    return Response.json(list);
  });
  const user = userEvent.setup();
  render(<ModuleShots moduleId={MOD} userName="小明" />);
  expect(await screen.findByText(/2 \/ 100 张/)).toBeInTheDocument();
  expect(screen.getByAltText("首页")).toHaveAttribute("src", `/api/modules/${MOD}/screenshots/s1`);
  // 上传一张 PNG；不是图片的会被挡在前端
  const file = new File([new Uint8Array([137, 80, 78, 71])], "新图.png", { type: "image/png" });
  await user.upload(screen.getByLabelText("选择 UI 图"), file);
  await waitFor(() => expect(posts()).toHaveLength(1));
  expect(JSON.parse(String(posts()[0][1]?.body)).screenshots[0]).toMatchObject({ name: "新图.png" });
  expect(new Headers(posts()[0][1]?.headers).get("x-review-token")).toBe("tok-1234");
  expect(await screen.findByText(/新增 1 张/)).toBeInTheDocument();
  await userEvent.setup({ applyAccept: false }).upload(screen.getByLabelText("选择 UI 图"), new File(["x"], "a.gif", { type: "image/gif" }));
  expect(await screen.findByText(/不是 PNG \/ JPEG/)).toBeInTheDocument();
  expect(posts()).toHaveLength(1);
  // 删除：点卡片上的删除，再在确认框里确认
  await user.click(screen.getAllByRole("button", { name: /删\s*除/ })[0]);
  const confirm = await waitFor(() => {
    const popover = document.querySelector(".ant-popover:not(.ant-popover-hidden)") as HTMLElement | null;
    if (!popover) throw new Error("确认框还没出现");
    return within(popover).getByRole("button", { name: /删\s*除/ });
  });
  await user.click(confirm);
  await waitFor(() => expect(posts().some(([url]) => String(url).endsWith("/s1/delete"))).toBe(true));
});

test("UI 图：没填名字不发请求并提示", async () => {
  fetchMock.mockImplementation(async () => Response.json([]));
  const user = userEvent.setup();
  render(<ModuleShots moduleId={MOD} userName="" />);
  expect(await screen.findByText(/还没有 UI 图/)).toBeInTheDocument();
  await user.upload(screen.getByLabelText("选择 UI 图"), new File([new Uint8Array([137, 80, 78, 71])], "a.png", { type: "image/png" }));
  expect(await screen.findByText("请先填「我是谁」")).toBeInTheDocument();
  expect(posts()).toHaveLength(0);
});

test("测试用例：每条都逐条呈现（步骤、预期按行编号），运行环境有标签；有导出链接", async () => {
  const cases = [kase("T-1"), kase("T-2", { run_env: "h5", steps: ["打开链接"], expected: ["看到活动"] }), kase("T-3", { run_env: "browser" })];
  render(<ModuleCases moduleId={MOD} cases={cases} userName="小明" onChanged={() => {}} />);
  expect(screen.getByText("测试用例（共 3 条）")).toBeInTheDocument();
  for (const code of ["T-1", "T-2", "T-3"]) expect(screen.getByText(code)).toBeInTheDocument();
  expect(screen.getByText("App")).toBeInTheDocument();
  expect(screen.getByText("H5")).toBeInTheDocument();
  expect(screen.getByText("浏览器")).toBeInTheDocument();
  expect(screen.getAllByText("点充值").length).toBe(2);
  expect(screen.getByRole("link", { name: "导出 CSV" })).toHaveAttribute("href", `/api/modules/${MOD}/cases.csv`);
});

test("测试用例：导入 CSV 发给服务端，成功显示条数并刷新；服务端说哪一行有问题就原样显示", async () => {
  const onChanged = vi.fn();
  let reject = true;
  fetchMock.mockImplementation(async () =>
    reject
      ? new Response("CSV 里有问题，整个文件都没有导入：第 3 行：步骤不能为空", { status: 400 })
      : Response.json({ inserted: 2, updated: 1, unchanged: 0 }),
  );
  const user = userEvent.setup();
  render(<ModuleCases moduleId={MOD} cases={[]} userName="小明" onChanged={onChanged} />);
  await user.click(screen.getByRole("button", { name: /导入 CSV/ }));
  const dialog = within(await screen.findByRole("dialog"));
  await user.click(dialog.getByLabelText("CSV 内容"));
  await user.paste("编号,标题,步骤,预期\nA,b,点,成");
  await user.click(dialog.getByRole("button", { name: /导\s*入/ }));
  expect(await dialog.findByText(/第 3 行：步骤不能为空/)).toBeInTheDocument();
  expect(onChanged).not.toHaveBeenCalled();
  reject = false;
  await user.click(dialog.getByRole("button", { name: /导\s*入/ }));
  expect(await dialog.findByText("导入完成：新增 2 条，更新 1 条，没变 0 条")).toBeInTheDocument();
  expect(onChanged).toHaveBeenCalled();
  expect(JSON.parse(String(posts().at(-1)?.[1]?.body)).csv).toContain("编号,标题,步骤,预期");
});

test("测试结果：每条一行，没跑过写「未执行」，汇总各类条数，未执行不算通过", () => {
  const run = (final_result: "passed" | "failed" | "uncertain" | "blocked" | null) => ({ run_id: "r1", task_id: "t1", status: "finished", final_result, blocked_reason: null, queued_at: "2026-11-24T00:00:00Z" });
  render(<ModuleResults cases={[kase("T-1", { latest_run: run("passed") }), kase("T-2", { latest_run: run("failed") }), kase("T-3"), kase("T-4", { latest_run: run(null) })]} />);
  expect(screen.getByText("通过 1")).toBeInTheDocument();
  expect(screen.getByText("失败 1")).toBeInTheDocument();
  expect(screen.getByText("未执行 1")).toBeInTheDocument();
  expect(screen.getByText("进行中 1")).toBeInTheDocument();
  expect(screen.getAllByRole("link", { name: "查看" }).length).toBe(3);
});
