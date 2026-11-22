import { Alert } from "antd";
import type { ReactNode } from "react";
import type { BlockedReason, FinalResult, JudgeStatus, RunStatus, RunView, Verdict } from "./api";
import { hasDisagreement, parseSummary, type VerdictCounts } from "./summary";
import { REFRESH_MS } from "./usePolling";

export { hasDisagreement, parseSummary, type VerdictCounts };

export type PillKind = "passed" | "failed" | "uncertain" | "blocked" | "running" | "judging" | "queued" | "cancelled" | "lost" | "info";

// 状态徽标：圆点 + 文字，颜色之外始终带文字，浅深色都可读
export function Pill({ kind, large, title, children }: { kind: PillKind; large?: boolean; title?: string; children: ReactNode }) {
  return (
    <span className={`status-pill pill-${kind}${large ? " status-pill-lg" : ""}`} title={title}>
      <span className="status-dot" aria-hidden="true" />
      {children}
    </span>
  );
}

export const STATUS: Record<RunStatus, { text: string; kind: PillKind }> = {
  queued: { text: "排队中", kind: "queued" },
  assigned: { text: "已分配", kind: "queued" },
  running: { text: "执行中", kind: "running" },
  judging: { text: "判定中", kind: "judging" },
  finished: { text: "已结束", kind: "queued" },
  blocked: { text: "受阻", kind: "blocked" },
  cancelled: { text: "已取消", kind: "cancelled" },
  node_lost: { text: "节点掉线", kind: "lost" },
};

const verdicts: Record<FinalResult, { text: string; kind: PillKind }> = {
  passed: { text: "通过", kind: "passed" },
  failed: { text: "失败", kind: "failed" },
  uncertain: { text: "待人工确认", kind: "uncertain" },
  blocked: { text: "受阻", kind: "blocked" },
};

const blocked: Record<BlockedReason, string> = {
  setup: "环境准备或恢复失败",
  link: "执行链路异常",
  node_lost: "节点断开连接",
  stuck: "执行卡住",
  quota: "模型额度用完",
  other: "执行未能完成",
};

export function blockedLabel(reason: BlockedReason) {
  return blocked[reason];
}

export function isTerminal(run: RunView) {
  return run.status === "finished" || run.status === "blocked" || run.status === "cancelled";
}

export function StatusPill({ status }: { status: RunStatus }) {
  return <Pill kind={STATUS[status].kind}>{STATUS[status].text}</Pill>;
}

export function VerdictTag({ verdict }: { verdict: Verdict }) {
  return <Pill kind={verdicts[verdict].kind}>{verdicts[verdict].text}</Pill>;
}

export function FinalTag({ run, large }: { run: RunView; large?: boolean }) {
  const result: { text: string; kind: PillKind } = run.final_result
    ? verdicts[run.final_result]
    : run.status === "cancelled"
      ? STATUS.cancelled
      : run.status === "finished"
        ? verdicts.uncertain
        : { text: "进行中", kind: "running" };
  return <Pill kind={result.kind} large={large}>{result.text}</Pill>;
}

export function JudgeTag({ status }: { status: JudgeStatus | null }) {
  return status === "completed"
    ? <Pill kind="info">Judge 已完成</Pill>
    : <Pill kind="uncertain" title={status === "failed" ? "Judge 判定失败" : "尚无判定结果"}>Judge 未完成</Pill>;
}

/** 结果分段条：通过 / 失败 / 待确认 / 受阻 / 进行中的比例，旁边的文字由调用方给出 */
export function SegBar({ counts, width = 148 }: { counts: VerdictCounts; width?: number }) {
  const parts: [keyof VerdictCounts, string][] = [["passed", "seg-passed"], ["failed", "seg-failed"], ["uncertain", "seg-uncertain"], ["blocked", "seg-blocked"], ["running", "seg-running"], ["cancelled", "seg-cancelled"]];
  const shown = parts.filter(([k]) => counts[k] > 0);
  return (
    <div className="seg-bar" style={{ width }} aria-hidden="true">
      {shown.length ? shown.map(([k, cls]) => <div key={k} className={cls} style={{ flex: counts[k] }} />) : <div className="seg-cancelled" style={{ flex: 1 }} />}
    </div>
  );
}

export function LoadError({ title, error, interval = REFRESH_MS }: { title: string; error: string; interval?: number }) {
  return <Alert type="error" showIcon title={title} description={`${error}。每 ${interval / 1000} 秒自动重试。`} />;
}

export function formatDate(date: string) {
  return new Date(date).toLocaleString("zh-CN", { hour12: false });
}

export function formatTime(date: string) {
  return new Date(date).toLocaleTimeString("zh-CN", { hour12: false });
}

export function formatDuration(run: RunView) {
  if (!run.started_at) return "—";
  const end = run.finished_at ? Date.parse(run.finished_at) : Date.now();
  const seconds = Math.max(0, Math.floor((end - Date.parse(run.started_at)) / 1000));
  return seconds >= 60 ? `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒` : `${seconds} 秒`;
}
