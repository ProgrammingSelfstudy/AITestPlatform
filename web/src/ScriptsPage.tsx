import IdentityHint from "./IdentityHint";
import { NEED_LOGIN_MESSAGE, useRememberedUserName } from "./identity";
import { Button, Empty, Input, Modal, Segmented, Select, Skeleton, Space, Table, Typography } from "antd";
import type { ColumnsType } from "antd/es/table";
import { useCallback, useMemo, useState } from "react";
import { cancelRegression, decideScript, fetchNodes, fetchScripts, requestRegression, type RegressionRequest, type ScriptRow, type ScriptStatus, type Triage } from "./api";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";
import { formatDate, LoadError, Pill, type PillKind } from "./runUi";
import { usePolling } from "./usePolling";

const PROJECT = "hafa";
const REFRESH_MS = 5000;

// 脚本状态不是用例结论：「已获准」只表示这个版本可以被平台选来回归，不是测试通过
const STATUS: Record<ScriptStatus, { kind: PillKind; text: string }> = {
  candidate: { kind: "queued", text: "候选，待自检" },
  self_check_passed: { kind: "info", text: "自检通过，待获准" },
  approved: { kind: "passed", text: "已获准" },
  rejected: { kind: "failed", text: "已拒绝" },
  retired: { kind: "cancelled", text: "已下线" },
};

type Action = "approve" | "retire" | "reject";

const ACTION: Record<Action, { label: string; verb: string; required: boolean; hint: string }> = {
  approve: { label: "获准", verb: "获准", required: false, hint: "获准后平台可以选这个版本做回归。同一用例同时只能有一个获准版本。可以写下获准的依据（选填）。" },
  retire: { label: "下线", verb: "下线", required: true, hint: "下线后这个版本不能再被选来回归，且不能恢复。请写明原因。" },
  reject: { label: "拒绝", verb: "拒绝", required: true, hint: "拒绝后这个版本不能再使用，且不能恢复。请写明原因。" },
};

// 状态机在服务端：这里只决定显示哪些按钮，不能跳过自检直接获准
const ACTIONS: Record<ScriptStatus, Action[]> = {
  candidate: ["reject"],
  self_check_passed: ["approve", "reject"],
  approved: ["retire"],
  rejected: [],
  retired: [],
};

// 回归请求的状态：「已完成」只表示得到了回归结论，通过还是失败看回归结果
const REQUEST: Record<RegressionRequest["status"], { kind: PillKind; text: string }> = {
  queued: { kind: "queued", text: "排队，等节点领取" },
  running: { kind: "running", text: "节点正在回归" },
  finished: { kind: "info", text: "已完成" },
  failed: { kind: "blocked", text: "没得到结论" },
  cancelled: { kind: "cancelled", text: "已取消" },
};

// 自愈分诊的结论。「定位失效」只产出候选，不代表已经修好；「业务行为错误」说明失败是对的
const TRIAGE: Record<Triage["verdict"], { kind: PillKind; text: (t: Triage) => string }> = {
  locator_broken: { kind: "info", text: (t) => `定位失效，已产出修复候选 v${t.healed_version ?? "?"}` },
  business_bug: { kind: "failed", text: () => "疑似产品缺陷，待人处理" },
  unsure: { kind: "uncertain", text: () => "分不清，待人判断" },
};

const MODEL = { claude_cli: "Claude 命令行", codex_exec: "Codex" } as const;

type StatusFilter = "all" | ScriptStatus;
type ViewFilter = "all" | "attention" | "reg_failed" | "triage_bug" | "triage_unsure";
type SummaryTone = "pass" | "info" | "fail" | "unc";

interface Pending {
  script: ScriptRow;
  action: Action;
}

/** 发起回归或取消排队：先选自己的节点（取消不用），再填「我是谁」（名字，记在本浏览器 30 天）。 */
interface RegressPending {
  script: ScriptRow;
  cancel: boolean;
}

interface SummaryCard {
  key: string;
  label: string;
  count: number;
  note: string;
  tone: SummaryTone;
  active: boolean;
  onToggle: () => void;
}

/** 回归请求是否仍在排队或节点运行中 */
function isRequestActive(row: ScriptRow): boolean {
  const status = row.last_request?.status;
  return status === "queued" || status === "running";
}

/** 最近一次回归请求是否未得到结论，且晚于已有回归记录 */
function hasFailedRequest(row: ScriptRow): boolean {
  const q = row.last_request;
  const r = row.last_regression;
  return Boolean(q && q.status === "failed" && (!r || Date.parse(q.requested_at) > Date.parse(r.created_at)));
}

/** 分诊结论只能挂在它针对的那次失败上（regression_id 等于 last_regression.id 且该次回归失败） */
function visibleTriage(row: ScriptRow): Triage | null {
  if (isRequestActive(row)) return null;
  const r = row.last_regression;
  if (!r || r.passed || !row.last_triage) return null;
  return row.last_triage.regression_id === r.id ? row.last_triage : null;
}

/** 当前展示的最近一次回归是否为失败 */
function hasRecentFailure(row: ScriptRow): boolean {
  return Boolean(!isRequestActive(row) && row.last_regression && !row.last_regression.passed);
}

/** 需要测试人员优先关注或决策的脚本：待获准、回归失败、请求未得结论或存在有效分诊 */
function needsAttention(row: ScriptRow): boolean {
  return row.status === "self_check_passed" || hasRecentFailure(row) || hasFailedRequest(row) || visibleTriage(row) !== null;
}

/** 默认排序权重：异常失败与待人工判断置顶，待获准次之，历史归档沉底 */
function rowPriority(row: ScriptRow): number {
  const triage = visibleTriage(row);
  if (triage?.verdict === "business_bug") return 0;
  if (triage?.verdict === "unsure") return 1;
  if (hasRecentFailure(row)) return 2;
  if (hasFailedRequest(row)) return 3;
  if (row.status === "self_check_passed") return 4;
  if (row.status === "approved") return 5;
  if (row.status === "candidate") return 6;
  return 7;
}

/** 行级语义高亮类名：失败用红、待判断用琥珀、待获准用蓝，绝不将失败或待判断标成通过绿 */
function rowClassName(row: ScriptRow): string {
  const triage = visibleTriage(row);
  if (triage?.verdict === "business_bug") return "script-row-failed";
  if (triage?.verdict === "unsure") return "script-row-uncertain";
  if (triage?.verdict === "locator_broken" || hasRecentFailure(row)) return "script-row-failed";
  if (hasFailedRequest(row)) return "script-row-uncertain";
  if (row.status === "self_check_passed") return "script-row-review";
  if (row.status === "rejected" || row.status === "retired") return "script-row-muted";
  return "";
}

/** 紧凑日期：同一年省去年份以节约列宽，悬停或展开行可查看完整时间 */
function formatCompactDate(date: string): string {
  const parsed = new Date(date);
  const sameYear = parsed.getFullYear() === new Date().getFullYear();
  return parsed.toLocaleString("zh-CN", {
    ...(sameYear ? {} : { year: "numeric" }),
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

function matchesView(row: ScriptRow, view: ViewFilter): boolean {
  switch (view) {
    case "all":
      return true;
    case "attention":
      return needsAttention(row);
    case "reg_failed":
      return hasRecentFailure(row);
    case "triage_bug":
      return visibleTriage(row)?.verdict === "business_bug";
    case "triage_unsure":
      return visibleTriage(row)?.verdict === "unsure";
  }
}

export default function ScriptsPage() {
  const load = useCallback((signal: AbortSignal) => fetchScripts(PROJECT, signal), []);
  const { data, error, reload } = usePolling(load, REFRESH_MS);
  const [pending, setPending] = useState<Pending | null>(null);
  const [reason, setReason] = useState("");
  const [askToken, setAskToken] = useState(false);
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [regress, setRegress] = useState<RegressPending | null>(null);
  const [regressNode, setRegressNode] = useState<string | undefined>();
  const { name: userName, setName: setUserName, expiresAt: nameExpiresAt, forget: forgetName } = useRememberedUserName();
  const [regressToken, setRegressToken] = useState(false);
  const [statusFilter, setStatusFilter] = useState<StatusFilter>("all");
  const [viewFilter, setViewFilter] = useState<ViewFilter>("all");
  const loadNodes = useCallback((signal: AbortSignal) => fetchNodes(signal), []);
  const { data: nodes } = usePolling(loadNodes, REFRESH_MS);

  const scriptById = useMemo(() => new Map((data ?? []).map((s) => [s.id, s])), [data]);

  const counts = useMemo(() => {
    const list = data ?? [];
    let approved = 0;
    let selfCheckPassed = 0;
    let regFailed = 0;
    let triageBug = 0;
    let triageUnsure = 0;
    let attention = 0;
    for (const row of list) {
      if (row.status === "approved") approved += 1;
      if (row.status === "self_check_passed") selfCheckPassed += 1;
      if (hasRecentFailure(row)) regFailed += 1;
      const triage = visibleTriage(row);
      if (triage?.verdict === "business_bug") triageBug += 1;
      if (triage?.verdict === "unsure") triageUnsure += 1;
      if (needsAttention(row)) attention += 1;
    }
    return { approved, selfCheckPassed, regFailed, triageBug, triageUnsure, attention };
  }, [data]);

  // 先按状态与关注视图筛选，再按处理优先级稳定排序
  const filteredRows = useMemo(() => {
    return [...(data ?? [])]
      .filter((row) => (statusFilter === "all" || row.status === statusFilter) && matchesView(row, viewFilter))
      .sort((a, b) => rowPriority(a) - rowPriority(b));
  }, [data, statusFilter, viewFilter]);

  function resetFilters() {
    setStatusFilter("all");
    setViewFilter("all");
  }

  function toggleStatusSummary(target: ScriptStatus) {
    if (statusFilter === target && viewFilter === "all") {
      resetFilters();
    } else {
      setStatusFilter(target);
      setViewFilter("all");
    }
  }

  function toggleViewSummary(target: ViewFilter) {
    if (viewFilter === target && statusFilter === "all") {
      resetFilters();
    } else {
      setStatusFilter("all");
      setViewFilter(target);
    }
  }

  const summaryCards: SummaryCard[] = [
    {
      key: "approved",
      label: "已获准",
      count: counts.approved,
      note: "可发起真机回归",
      tone: "pass",
      active: statusFilter === "approved" && viewFilter === "all",
      onToggle: () => toggleStatusSummary("approved"),
    },
    {
      key: "self_check_passed",
      label: "待获准（自检通过）",
      count: counts.selfCheckPassed,
      note: "等待人工审核",
      tone: "info",
      active: statusFilter === "self_check_passed" && viewFilter === "all",
      onToggle: () => toggleStatusSummary("self_check_passed"),
    },
    {
      key: "reg_failed",
      label: "最近回归失败",
      count: counts.regFailed,
      note: "需排查退化或失效",
      tone: "fail",
      active: viewFilter === "reg_failed" && statusFilter === "all",
      onToggle: () => toggleViewSummary("reg_failed"),
    },
    {
      key: "triage_bug",
      label: "疑似产品缺陷",
      count: counts.triageBug,
      note: "分诊指向业务错误",
      tone: "fail",
      active: viewFilter === "triage_bug" && statusFilter === "all",
      onToggle: () => toggleViewSummary("triage_bug"),
    },
    {
      key: "triage_unsure",
      label: "分不清",
      count: counts.triageUnsure,
      note: "待人工介入判断",
      tone: "unc",
      active: viewFilter === "triage_unsure" && statusFilter === "all",
      onToggle: () => toggleViewSummary("triage_unsure"),
    },
  ];

  function openRegress(script: ScriptRow, cancel: boolean) {
    setActionError(null);
    setRegressNode(undefined);
    setRegressToken(false);
    setRegress({ script, cancel });
  }

  function closeRegress() {
    setRegress(null);
    setRegressToken(false);
  }

  async function submitRegress(enteredToken?: string) {
    if (!regress) return;
    const personal = userName.trim();
    if (!regress.cancel && !regressNode) {
      setActionError("请选择在线节点");
      return;
    }
    if (!personal) {
      setActionError("请填「我是谁」（你的名字）");
      return;
    }
    const token = enteredToken ?? readReviewToken();
    const step = writeTokenAction(!busy, token);
    if (step === "blocked") return;
    if (step === "prompt") {
      setRegressToken(true);
      return;
    }
    if (enteredToken !== undefined) rememberReviewToken(enteredToken);
    setRegressToken(false);
    setBusy(true);
    setActionError(null);
    try {
      if (regress.cancel) {
        await cancelRegression(regress.script.last_request!.id, token, personal);
      } else {
        await requestRegression(regress.script.id, regressNode!, token, personal);
      }
      closeRegress();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
      reload();
    }
  }

  function open(script: ScriptRow, action: Action) {
    setActionError(null);
    setReason("");
    setPending({ script, action });
  }

  function close() {
    setPending(null);
    setAskToken(false);
  }

  async function submit(enteredToken?: string) {
    if (!pending) return;
    const text = reason.trim();
    if (ACTION[pending.action].required && !text) {
      setActionError(`${ACTION[pending.action].verb}要写明原因`);
      return;
    }
    const token = enteredToken ?? readReviewToken();
    const step = writeTokenAction(!busy, token);
    if (step === "blocked") return;
    if (step === "prompt") {
      setAskToken(true);
      return;
    }
    if (enteredToken !== undefined) rememberReviewToken(enteredToken);
    setAskToken(false);
    setBusy(true);
    setActionError(null);
    try {
      await decideScript(pending.script.id, pending.action, text, token);
      close();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
      reload();
    }
  }

  const columns: ColumnsType<ScriptRow> = [
    {
      title: "用例与版本",
      key: "case",
      width: 204,
      render: (_, row) => {
        const parent = row.healed_from ? scriptById.get(row.healed_from) : undefined;
        return (
          <div className="script-case-cell">
            <div className="script-case-head">
              <span className="code-tag">{row.case_code}</span>
              <span className="mono script-version">v{row.version}</span>
              {row.healed_from && (
                <span className="script-heal-tag" title="自愈产出的候选，要重新自检、人工获准">
                  修自 v{parent?.version ?? "?"}
                </span>
              )}
            </div>
            <div className="script-case-title" title={row.case_title}>{row.case_title}</div>
          </div>
        );
      },
    },
    {
      title: "状态",
      dataIndex: "status",
      width: 146,
      render: (status: ScriptStatus) => <Pill kind={STATUS[status].kind}>{STATUS[status].text}</Pill>,
    },
    {
      title: "回归与分诊",
      key: "regression",
      width: 244,
      render: (_, row) => {
        const r = row.last_regression;
        const q = row.last_request;
        // 请求还没结束时优先显示请求状态；结束后显示回归结果（请求没得到结论时两个都显示，让人看到原因）
        const active = isRequestActive(row);
        const failedRequest = hasFailedRequest(row);
        const triage = visibleTriage(row);
        return (
          <div className="script-regression-cell">
            <div className="script-pill-stack">
              {active && q && <Pill kind={REQUEST[q.status].kind}>{REQUEST[q.status].text}</Pill>}
              {failedRequest && q && <Pill kind={REQUEST.failed.kind} title={q.detail}>{REQUEST.failed.text}</Pill>}
              {!active && r && <Pill kind={r.passed ? "passed" : "failed"}>{r.passed ? "回归通过" : "回归失败"}</Pill>}
              {triage && (
                <Pill kind={TRIAGE[triage.verdict].kind} title={triage.reason}>
                  {TRIAGE[triage.verdict].text(triage)}
                </Pill>
              )}
              {!active && !r && !failedRequest && (
                row.status === "approved"
                  ? <span className="script-muted">还没回归过</span>
                  : <span className="script-muted">—</span>
              )}
            </div>
            {!active && r && (
              <span className="script-meta-line mono">{formatDate(r.created_at)} · {r.serial}</span>
            )}
            {failedRequest && q?.detail && (
              <span className="script-error-note wrap-text">{q.detail}</span>
            )}
          </div>
        );
      },
    },
    {
      title: "来源与更新",
      key: "provenance",
      width: 174,
      render: (_, row) => (
        <div className="script-meta-cell">
          <div className="script-gen-line">
            <span className="script-gen-name" title={row.gen_model ?? MODEL[row.gen_source]}>
              {row.gen_model ?? MODEL[row.gen_source]}
            </span>
            {row.gen_effort && <span className="script-effort-tag mono">{row.gen_effort}</span>}
          </div>
          <div className="script-meta-line">
            <a className="run-code-link mono" href={`#/runs/${row.source_run_id}`} title={`查看来源执行 ${row.source_run_id}`}>
              {row.source_run_id.slice(0, 8)}
            </a>
            <span className="script-dot-sep" aria-hidden="true">·</span>
            <span className="mono" title={`更新于 ${formatDate(row.updated_at)}`}>{formatCompactDate(row.updated_at)}</span>
          </div>
        </div>
      ),
    },
    {
      title: "操作",
      key: "actions",
      width: 152,
      fixed: "right",
      align: "right",
      render: (_, row) => {
        const statusActions = ACTIONS[row.status];
        const canCancel = row.status === "approved" && row.last_request?.status === "queued";
        const canRegress = row.status === "approved" && !isRequestActive(row);
        if (!canCancel && !canRegress && statusActions.length === 0) {
          return <span className="script-muted">—</span>;
        }
        return (
          <Space size={6} className="script-actions">
            {canCancel && (
              <Button size="small" onClick={() => openRegress(row, true)}>取消回归</Button>
            )}
            {canRegress && (
              <Button size="small" type="primary" ghost onClick={() => openRegress(row, false)}>回归</Button>
            )}
            {statusActions.map((action) => (
              <Button
                key={action}
                size="small"
                type={action === "approve" ? "primary" : "default"}
                danger={action !== "approve"}
                onClick={() => open(row, action)}
              >
                {ACTION[action].label}
              </Button>
            ))}
          </Space>
        );
      },
    },
  ];

  const isFiltered = statusFilter !== "all" || viewFilter !== "all";

  return (
    <div className="scripts-page">
      <div className="page-heading">
        <div className="page-title-block">
          <Typography.Title level={3}>回归脚本</Typography.Title>
          <p className="page-desc">
            聚焦需要处理的回归失败、分诊结论与待获准版本；点击顶部摘要卡片或筛选条件可快速定位。
          </p>
          <details className="scripts-guide">
            <summary>查看流转规则与节点命令行说明</summary>
            <div className="scripts-guide-body">
              <p>
                <strong>生命周期：</strong>通过的执行在节点上运行 <code className="mono">node gen-script / self-check / heal</code> 生成与自检（候选 → 真机自检通过 → 人工获准）。同一用例同时只有一个已获准版本，获准只代表可被平台选来回归。
              </p>
              <p>
                <strong>回归与自愈：</strong>已获准版本可在自己的节点上发起「回归」（不花模型额度，但会占用该节点手机并排队领取）。回归失败不会改脚本状态；节点运行 <code className="mono">node heal</code> 后，仅确认定位失效才产出修复候选，业务错误保留失败，分不清交人工判断。
              </p>
            </div>
          </details>
        </div>
      </div>

      {error ? (
        <LoadError title="连不上平台服务端" error={error} interval={REFRESH_MS} />
      ) : !data ? (
        <Skeleton active />
      ) : data.length === 0 ? (
        <div className="card empty-state">
          <Empty description="还没有脚本：在节点上对一次通过的执行运行 node gen-script 生成候选" />
        </div>
      ) : (
        <>
          <section className="scripts-summary-grid" aria-label="脚本状态摘要">
            {summaryCards.map((item) => {
              const emphasize = item.count > 0 && (item.tone === "fail" || item.tone === "unc");
              return (
                <button
                  key={item.key}
                  type="button"
                  aria-pressed={item.active}
                  onClick={item.onToggle}
                  className={`surface scripts-summary-card${item.active ? " is-active" : ""}${emphasize ? ` is-alert-${item.tone}` : ""}`}
                >
                  <span className="scripts-summary-head">
                    <span className={`scripts-summary-dot${item.count > 0 ? ` dot-${item.tone}` : ""}`} aria-hidden="true" />
                    <span className="scripts-summary-label">{item.label}</span>
                  </span>
                  <span className={`scripts-summary-value${item.count > 0 ? ` val-${item.tone}` : " val-zero"}`}>
                    {item.count}
                  </span>
                  <span className="scripts-summary-note">{item.note}</span>
                </button>
              );
            })}
          </section>

          <section className="card table-card scripts-table-card" aria-label="回归脚本列表">
            <div className="card-header">
              <div className="scripts-card-title-group">
                <h2 className="card-title">
                  脚本列表（{filteredRows.length}{isFiltered ? ` / ${data.length}` : ""}）
                </h2>
                {counts.attention > 0 && !isFiltered && (
                  <span className="scripts-attention-hint">
                    {counts.attention} 项需要关注，已按优先级置顶
                  </span>
                )}
              </div>
              <div className="filter-controls">
                <Select<StatusFilter>
                  aria-label="按脚本状态筛选"
                  value={statusFilter}
                  onChange={setStatusFilter}
                  style={{ width: 164 }}
                  options={[
                    { value: "all", label: "全部状态" },
                    { value: "self_check_passed", label: "自检通过，待获准" },
                    { value: "approved", label: "已获准" },
                    { value: "candidate", label: "候选，待自检" },
                    { value: "rejected", label: "已拒绝" },
                    { value: "retired", label: "已下线" },
                  ]}
                />
                <Segmented<ViewFilter>
                  aria-label="按处理视图筛选"
                  value={viewFilter}
                  onChange={setViewFilter}
                  options={[
                    { value: "all", label: "全部" },
                    { value: "attention", label: counts.attention > 0 ? `待处理 (${counts.attention})` : "待处理" },
                    { value: "reg_failed", label: "回归失败" },
                    { value: "triage_bug", label: "疑似缺陷" },
                    { value: "triage_unsure", label: "分不清" },
                  ]}
                />
              </div>
            </div>
            <Table<ScriptRow>
              rowKey="id"
              columns={columns}
              dataSource={filteredRows}
              pagination={false}
              scroll={{ x: 920 }}
              rowClassName={rowClassName}
              locale={{
                emptyText: (
                  <div className="table-empty-box">
                    <Empty description="没有符合当前筛选条件的脚本" />
                    {isFiltered && (
                      <Button size="small" onClick={resetFilters}>重置筛选条件</Button>
                    )}
                  </div>
                ),
              }}
              expandable={{
                columnWidth: 38,
                expandedRowRender: (row) => (
                  <div className="script-detail">
                    <div className="script-detail-meta">
                      <p>
                        <strong>文件：</strong>
                        <span className="mono">{row.file_names.join("、")}</span>（依据用例版本 {row.case_version}）
                      </p>
                      <p>
                        <strong>更新时间：</strong>
                        <span className="mono">{formatDate(row.updated_at)}</span>
                      </p>
                    </div>
                    {row.notes && (
                      <p className="script-detail-block">
                        <strong>生成备注：</strong>{row.notes}
                      </p>
                    )}
                    {row.last_regression && (
                      <div className="script-detail-block">
                        <strong>
                          最近一次回归（{row.last_regression.passed ? "通过" : "失败，可能是产品退化，也可能是脚本随界面改版失效，需要人判断"}，用时 {(row.last_regression.duration_ms / 1000).toFixed(0)} 秒）：
                        </strong>
                        <pre className="script-note">{row.last_regression.detail}</pre>
                      </div>
                    )}
                    {row.last_triage && (
                      <div className="script-detail-block">
                        <strong>
                          自愈分诊（{TRIAGE[row.last_triage.verdict].text(row.last_triage)}，针对{row.last_triage.regression_id === row.last_regression?.id ? "最近一次" : "更早的一次"}失败）：
                        </strong>
                        <pre className="script-note">{row.last_triage.reason}</pre>
                      </div>
                    )}
                    {row.status_note && (
                      <div className="script-detail-block">
                        <strong>状态说明（自检输出和人的决定）：</strong>
                        <pre className="script-note">{row.status_note}</pre>
                      </div>
                    )}
                  </div>
                ),
              }}
            />
          </section>
        </>
      )}

      {pending && !askToken && (
        <Modal open title={`${ACTION[pending.action].label}：${pending.script.case_code} v${pending.script.version}`}
          okText={`确认${ACTION[pending.action].label}`} cancelText="取消" width={520}
          okButtonProps={{ danger: pending.action !== "approve", loading: busy }}
          onOk={() => submit()} onCancel={close}>
          <p>{ACTION[pending.action].hint}</p>
          <Input.TextArea aria-label="原因" rows={3} maxLength={300} showCount value={reason}
            placeholder={ACTION[pending.action].required ? "原因（必填）" : "原因（选填）"}
            onChange={(event) => setReason(event.target.value)} />
          {actionError && <Typography.Text type="danger">{actionError}</Typography.Text>}
        </Modal>
      )}
      {regress && !regressToken && (
        <Modal open title={`${regress.cancel ? "取消回归" : "回归"}：${regress.script.case_code} v${regress.script.version}`}
          okText={regress.cancel ? "确认取消" : "排队回归"} cancelText="关闭" width={520}
          okButtonProps={{ loading: busy }} onOk={() => submitRegress()} onCancel={closeRegress}>
          <p>
            {regress.cancel
              ? "只能取消还在排队的请求；节点已经领走、手机正在跑的取消不了。"
              : "请求会排在你选的节点上，由它在自己的手机上跑这个已获准的版本。只能用自己的节点。"}
          </p>
          {!regress.cancel && (
            <Select aria-label="节点" style={{ width: "100%", marginBottom: 12 }} placeholder="选择在线节点" value={regressNode}
              onChange={setRegressNode} disabled={!nodes || busy}
              options={nodes?.map((candidate) => ({
                value: candidate.node_id,
                label: `${candidate.node_id.slice(0, 8)} · ${candidate.online ? `在线，${candidate.devices.length} 台设备` : "离线，无法接收"}`,
                disabled: !candidate.online,
              })) ?? []} />
          )}
          <Input aria-label="我是谁" autoComplete="off" maxLength={40} placeholder="我是谁（你的名字，用来确认节点是你的）"
            value={userName} onChange={(event) => {
              setUserName(event.target.value);
              if (actionError === NEED_LOGIN_MESSAGE) setActionError(null);
            }} />
          <IdentityHint userName={userName} expiresAt={nameExpiresAt} nameProblem={actionError} onForget={forgetName}
            onLogin={() => setActionError(null)} />
          {actionError && <div><Typography.Text type="danger">{actionError}</Typography.Text></div>}
        </Modal>
      )}
      {regress && regressToken && (
        <ReviewTokenDialog disabled={busy} onConfirm={(token) => submitRegress(token)} onCancel={closeRegress} />
      )}
      {pending && askToken && (
        <ReviewTokenDialog disabled={busy} onConfirm={(token) => submit(token)} onCancel={close} />
      )}
    </div>
  );
}
