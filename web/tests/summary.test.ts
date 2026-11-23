import { expect, test } from "vitest";
import { hasDisagreement, parseSummary } from "../src/summary";
import type { FinalResult, JudgeStatus, Verdict } from "../src/api";

const zeroCounts = { passed: 0, failed: 0, uncertain: 0, blocked: 0, cancelled: 0, running: 0 };

test("解析通过 3、失败 1，其余数量为零", () => {
  expect(parseSummary("通过 3、失败 1")).toEqual({ ...zeroCounts, passed: 3, failed: 1 });
});

test("空串的所有数量均为零", () => {
  expect(parseSummary("")).toEqual(zeroCounts);
});

test("忽略未知片段，保留已知片段", () => {
  expect(parseSummary("跳过 9、未知片段、受阻 2")).toEqual({ ...zeroCounts, blocked: 2 });
});

test("忽略非法数字和缺少数量的片段", () => {
  expect(parseSummary("通过 -3、失败 1.5、待人工确认 1e2、受阻 无、已取消、进行中 2")).toEqual({ ...zeroCounts, running: 2 });
});

const agreedRun: { judge_status: JudgeStatus | null; executor_result: Verdict | null; final_result: FinalResult | null } = { judge_status: "completed", executor_result: "passed", final_result: "passed" };

test("Judge 未完成时不判为不一致", () => {
  for (const status of ["not_run", "failed", null] as const) {
    expect(hasDisagreement({ ...agreedRun, judge_status: status, final_result: "failed" }), `Judge 状态：${status ?? "空"}`).toBe(false);
  }
});

test("自评为空时不判为不一致", () => {
  expect(hasDisagreement({ ...agreedRun, executor_result: null })).toBe(false);
});

test("结论为空时不判为不一致", () => {
  expect(hasDisagreement({ ...agreedRun, final_result: null })).toBe(false);
});

test("自评与已完成的 Judge 结论相同时不判为不一致", () => {
  expect(hasDisagreement(agreedRun)).toBe(false);
});

test("自评与已完成的 Judge 结论不同时判为不一致", () => {
  expect(hasDisagreement({ ...agreedRun, final_result: "failed" })).toBe(true);
});
