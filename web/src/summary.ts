import type { RunView } from "./api";

export interface VerdictCounts { passed: number; failed: number; uncertain: number; blocked: number; cancelled: number; running: number }

/** 把服务端 result_summary（如「通过 3、失败 1」）拆成数量，用来画结果条；认不出的片段忽略 */
export function parseSummary(text: string): VerdictCounts {
  const map: Record<string, keyof VerdictCounts> = { 通过: "passed", 失败: "failed", 待人工确认: "uncertain", 受阻: "blocked", 已取消: "cancelled", 进行中: "running" };
  const counts: VerdictCounts = { passed: 0, failed: 0, uncertain: 0, blocked: 0, cancelled: 0, running: 0 };
  for (const part of text.split("、")) {
    const [label, numberText] = part.trim().split(/\s+/);
    if (label in map && /^\d+$/.test(numberText ?? "")) counts[map[label]] = Number(numberText);
  }
  return counts;
}

/** 自评与 Judge 结论不一致：只有 Judge 已完成且双方都有结论才算 */
export function hasDisagreement(run: RunView) {
  return run.judge_status === "completed" && !!run.executor_result && !!run.final_result && run.executor_result !== run.final_result;
}
