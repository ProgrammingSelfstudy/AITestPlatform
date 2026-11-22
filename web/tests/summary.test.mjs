import assert from "node:assert/strict";
import test from "node:test";
import { hasDisagreement, parseSummary } from "../src/summary.ts";

const zeroCounts = { passed: 0, failed: 0, uncertain: 0, blocked: 0, cancelled: 0, running: 0 };

test("解析通过 3、失败 1，其余数量为零", () => {
  assert.deepEqual(parseSummary("通过 3、失败 1"), { ...zeroCounts, passed: 3, failed: 1 });
});

test("空串的所有数量均为零", () => {
  assert.deepEqual(parseSummary(""), zeroCounts);
});

test("忽略未知片段，保留已知片段", () => {
  assert.deepEqual(parseSummary("跳过 9、未知片段、受阻 2"), { ...zeroCounts, blocked: 2 });
});

test("忽略非法数字和缺少数量的片段", () => {
  assert.deepEqual(parseSummary("通过 -3、失败 1.5、待人工确认 1e2、受阻 无、已取消、进行中 2"), { ...zeroCounts, running: 2 });
});

const agreedRun = { judge_status: "completed", executor_result: "passed", final_result: "passed" };

test("Judge 未完成时不判为不一致", () => {
  for (const status of ["not_run", "failed", null]) {
    assert.equal(hasDisagreement({ ...agreedRun, judge_status: status, final_result: "failed" }), false, `Judge 状态：${status ?? "空"}`);
  }
});

test("自评为空时不判为不一致", () => {
  assert.equal(hasDisagreement({ ...agreedRun, executor_result: null }), false);
});

test("结论为空时不判为不一致", () => {
  assert.equal(hasDisagreement({ ...agreedRun, final_result: null }), false);
});

test("自评与已完成的 Judge 结论相同时不判为不一致", () => {
  assert.equal(hasDisagreement(agreedRun), false);
});

test("自评与已完成的 Judge 结论不同时判为不一致", () => {
  assert.equal(hasDisagreement({ ...agreedRun, final_result: "failed" }), true);
});
