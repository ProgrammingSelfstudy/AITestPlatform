import { render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { JudgeOpinion, RunDetail } from "../src/api";
import RunPage from "../src/RunPage";

const fetchMock = vi.fn<typeof fetch>();
const RUN = "cccccccc-0000-0000-0000-000000000001";

const item = (idx: number, verdict: "passed" | "failed", evidence: string) => ({ idx, expected_text: `预期${idx}`, verdict, evidence, file_refs: [] as string[] });
const opinion = (seq: number, model: string, final_result: JudgeOpinion["final_result"], items = [item(1, "passed", `判定员${seq}的证据`)], judge_status: JudgeOpinion["judge_status"] = "completed"): JudgeOpinion =>
  ({ seq, source: "claude_cli", model, effort: null, judge_status, final_result, items });

function run(opinions: JudgeOpinion[] | undefined, final_result: RunDetail["final_result"]): RunDetail {
  return {
    run_id: RUN, task_id: "t1", code: "TC-1", case_version: 1, status: "finished", executor_result: "passed", judge_status: "completed", final_result, blocked_reason: null,
    queued_at: "2026-11-24T00:00:00Z", started_at: "2026-11-24T00:00:01Z", finished_at: "2026-11-24T00:01:00Z", device_serial: "D1",
    case_spec: { code: "TC-1", title: "用例", steps: ["点一下"], expected: [{ idx: 1, text: "预期1" }] },
    executor_reason: null, cost: null, run_steps: [], judge_items: [item(1, "passed", "主判定员的证据")], judge_opinions: opinions,
    files: [], file_urls: {}, recordings: [], evidence_expires_at: 9999999999, evidence_key_id: "k",
  } as unknown as RunDetail;
}

/** 运行详情和复核记录是两个接口：复核记录没有数据就返回空 */
function serve(detail: RunDetail) {
  fetchMock.mockImplementation(async (input) =>
    String(input).includes("/reviews") ? Response.json({ history: [], current: [] }) : Response.json(detail),
  );
}

beforeEach(() => vi.stubGlobal("fetch", fetchMock));
afterEach(() => { vi.unstubAllGlobals(); fetchMock.mockReset(); });

test("多个判定员：每个人各自的结论单独列出，有分歧时整体是待人工确认", async () => {
  serve(run([
    opinion(0, "opus", "passed"),
    opinion(1, "gemini-x", "failed", [item(1, "failed", "判定员 2 看到按钮没出现")]),
    opinion(2, "sonnet", "uncertain", [], "failed"),
  ], "uncertain"));
  render(<RunPage id={RUN} />);
  const panel = within(await screen.findByLabelText("各判定员的结果"));
  expect(panel.getByText(/3 个判定员同时评审同一份证据/)).toBeInTheDocument();
  expect(panel.getByText(/主判定员 · .*opus/)).toBeInTheDocument();
  expect(panel.getByText(/判定员 2 · .*gemini-x/)).toBeInTheDocument();
  expect(panel.getByText(/判定员 3 · .*sonnet/)).toBeInTheDocument();
  expect(panel.getByText("判定员 2 看到按钮没出现", { exact: false })).toBeInTheDocument();
  // 三种结论各自显示，没跑成的明确写出来，不替他编结论
  expect(panel.getAllByText("通过").length).toBeGreaterThan(0);
  expect(panel.getAllByText("失败").length).toBeGreaterThan(0);
  expect(panel.getByText("评审没跑成")).toBeInTheDocument();
  expect(panel.getByText("这个判定员没有给出逐条结论。")).toBeInTheDocument();
});

test("只有一个判定员的 run：不显示「各判定员的结果」", async () => {
  serve(run([], "passed"));
  const { container } = render(<RunPage id={RUN} />);
  expect(await screen.findByText(/主判定员的证据/)).toBeInTheDocument();
  expect(container.querySelector('[aria-label="各判定员的结果"]')).toBeNull();
});

test("老接口没有 judge_opinions 字段也不报错", async () => {
  serve(run(undefined, "passed"));
  const { container } = render(<RunPage id={RUN} />);
  expect(await screen.findByText(/主判定员的证据/)).toBeInTheDocument();
  expect(container.querySelector('[aria-label="各判定员的结果"]')).toBeNull();
});
