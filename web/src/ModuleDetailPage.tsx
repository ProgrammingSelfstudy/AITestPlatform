import { Alert, Button, Empty, Input, Modal, Skeleton } from "antd";
import type { ComponentType } from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  approveReviewed,
  decideCase,
  fetchModuleCases,
  fetchModules,
  type LatestRun,
  type ModuleCase,
  type ModuleView,
  type ReviewStatus,
} from "./api";
import NewTaskDialog from "./NewTaskDialog";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";
import { formatDate, formatTime, LoadError } from "./runUi";
import { REFRESH_MS, usePolling } from "./usePolling";
import IdentityHint from "./IdentityHint";
import { useRememberedUserName } from "./identity";
import "./modules.css";

const project = "hafa";

// NewTaskDialog 将新增 initialCases?: string[] 属性；利用函数参数逆变保持严格模式兼容
const TaskDialogWithCases: ComponentType<{
  onClose: () => void;
  initialCases?: string[];
}> = NewTaskDialog;

type BadgeTone = "pass" | "fail" | "unc" | "blk" | "run" | "que" | "can" | "lost" | "reviewed";

/**
 * 复核状态文案与语义色：
 * - reviewed_ok 绝不用绿色，也不是「通过」，明确写成「评审员认为没问题（待你确认）」
 * - needs_human 用醒目橙色
 * - 只有 approved 用绿色「已批准」
 */
const REVIEW_META: Record<ReviewStatus, { text: string; tone: BadgeTone }> = {
  draft: { text: "草稿", tone: "que" },
  reviewed_ok: { text: "评审员认为没问题（待你确认）", tone: "reviewed" },
  needs_human: { text: "需要人工确认", tone: "unc" },
  approved: { text: "已批准", tone: "pass" },
  rejected: { text: "已拒绝", tone: "fail" },
};

const RUN_STATUS_META: Record<string, { text: string; tone: BadgeTone; active: boolean }> = {
  queued: { text: "排队中", tone: "que", active: true },
  assigned: { text: "已分配", tone: "que", active: true },
  running: { text: "执行中", tone: "run", active: true },
  judging: { text: "判定中", tone: "run", active: true },
  finished: { text: "已结束", tone: "que", active: false },
  blocked: { text: "受阻", tone: "blk", active: false },
  cancelled: { text: "已取消", tone: "can", active: false },
  node_lost: { text: "节点掉线", tone: "lost", active: false },
};

const FINAL_RESULT_META: Record<
  NonNullable<LatestRun["final_result"]>,
  { text: string; tone: BadgeTone }
> = {
  passed: { text: "通过", tone: "pass" },
  failed: { text: "失败", tone: "fail" },
  uncertain: { text: "待人工确认", tone: "unc" },
  blocked: { text: "受阻", tone: "blk" },
};

const BLOCKED_REASON_TEXT: Record<string, string> = {
  setup: "环境准备或恢复失败",
  link: "执行链路异常",
  node_lost: "节点断开连接",
  stuck: "执行卡住",
  quota: "模型额度用完",
  other: "执行未能完成",
};

function getRunStatusMeta(status: string) {
  return RUN_STATUS_META[status] ?? { text: status, tone: "que" as BadgeTone, active: false };
}

function formatBlockedReason(reason: string | null): string | null {
  if (!reason) return null;
  return BLOCKED_REASON_TEXT[reason] ?? reason;
}

function isRunActive(run: LatestRun | null): boolean {
  if (!run) return false;
  return (
    run.status === "queued" ||
    run.status === "assigned" ||
    run.status === "running" ||
    run.status === "judging"
  );
}

function getCaseRunDisplay(run: LatestRun | null): {
  text: string;
  tone: BadgeTone;
  cardClass: string;
  meterClass: string;
  pulse: boolean;
} {
  if (!run) {
    return {
      text: "暂无执行记录",
      tone: "que",
      cardClass: "",
      meterClass: "mod-meter-que",
      pulse: false,
    };
  }
  if (run.status === "running" || run.status === "judging") {
    const meta = getRunStatusMeta(run.status);
    return {
      text: meta.text,
      tone: "run",
      cardClass: " is-running",
      meterClass: "mod-meter-run",
      pulse: true,
    };
  }
  if (run.status === "queued" || run.status === "assigned") {
    const meta = getRunStatusMeta(run.status);
    return {
      text: meta.text,
      tone: "que",
      cardClass: "",
      meterClass: "mod-meter-que",
      pulse: false,
    };
  }
  if (run.final_result) {
    const res = FINAL_RESULT_META[run.final_result];
    if (run.final_result === "passed") {
      return {
        text: res.text,
        tone: "pass",
        cardClass: "",
        meterClass: "mod-meter-pass",
        pulse: false,
      };
    }
    if (run.final_result === "failed") {
      return {
        text: res.text,
        tone: "fail",
        cardClass: " is-failed",
        meterClass: "mod-meter-fail",
        pulse: false,
      };
    }
    if (run.final_result === "uncertain") {
      return {
        text: res.text,
        tone: "unc",
        cardClass: " is-uncertain",
        meterClass: "mod-meter-unc",
        pulse: false,
      };
    }
    return {
      text: res.text,
      tone: "blk",
      cardClass: "",
      meterClass: "mod-meter-blk",
      pulse: false,
    };
  }
  if (run.status === "finished") {
    return {
      text: "待人工确认",
      tone: "unc",
      cardClass: " is-uncertain",
      meterClass: "mod-meter-unc",
      pulse: false,
    };
  }
  const fallback = getRunStatusMeta(run.status);
  return {
    text: fallback.text,
    tone: fallback.tone,
    cardClass: "",
    meterClass: fallback.tone === "lost" ? "mod-meter-unc" : "mod-meter-blk",
    pulse: false,
  };
}

type StepVisual = "done" | "active" | "error" | "pending";

interface FlowStepItem {
  key: string;
  num: number;
  label: string;
  state: StepVisual;
  stateText: string;
  errorDetail?: string;
}

function buildFlowSteps(mod: ModuleView, cases: ModuleCase[]): FlowStepItem[] {
  const req = mod.latest_request;
  const reqFailed = req?.status === "failed";
  const reqRunning = req?.status === "queued" || req?.status === "running";
  const reqCancelled = req?.status === "cancelled";

  const hasCases = mod.case_count > 0 || cases.length > 0;
  const hasDraft = cases.some((c) => c.review_status === "draft");
  const hasPendingConfirm = cases.some(
    (c) => c.review_status === "reviewed_ok" || c.review_status === "needs_human",
  );
  const hasAnyRun = cases.some((c) => c.latest_run !== null);

  // 1. 提交
  const step1: FlowStepItem = {
    key: "submit",
    num: 1,
    label: "提交",
    state: "done",
    stateText: "已提交",
  };

  // 2. 生成
  let step2: FlowStepItem;
  if (reqFailed) {
    step2 = {
      key: "generate",
      num: 2,
      label: "生成",
      state: "error",
      stateText: "生成失败",
      errorDetail: req?.detail || "服务端未返回失败原因",
    };
  } else if (reqRunning) {
    step2 = {
      key: "generate",
      num: 2,
      label: "生成",
      state: "active",
      stateText: req?.status === "queued" ? "排队中" : "生成中",
    };
  } else if (hasCases || req?.status === "finished") {
    step2 = {
      key: "generate",
      num: 2,
      label: "生成",
      state: "done",
      stateText: "已完成",
    };
  } else if (reqCancelled) {
    step2 = {
      key: "generate",
      num: 2,
      label: "生成",
      state: "error",
      stateText: "已取消",
      errorDetail: req?.detail || undefined,
    };
  } else {
    step2 = {
      key: "generate",
      num: 2,
      label: "生成",
      state: "pending",
      stateText: "等待生成",
    };
  }

  // 3. 复核（评审员复核）
  let step3: FlowStepItem;
  if (!hasCases || reqRunning || reqFailed) {
    step3 = {
      key: "review",
      num: 3,
      label: "复核",
      state: "pending",
      stateText: "等待复核",
    };
  } else if (hasDraft) {
    step3 = {
      key: "review",
      num: 3,
      label: "复核",
      state: "active",
      stateText: "复核中",
    };
  } else {
    step3 = {
      key: "review",
      num: 3,
      label: "复核",
      state: "done",
      stateText: "评审员已复核",
    };
  }

  // 4. 确认（人工确认）
  let step4: FlowStepItem;
  if (!hasCases || reqRunning || reqFailed || hasDraft) {
    step4 = {
      key: "confirm",
      num: 4,
      label: "确认",
      state: "pending",
      stateText: "等待确认",
    };
  } else if (mod.active_runs > 0) {
    step4 = {
      key: "confirm",
      num: 4,
      label: "确认",
      state: "done",
      stateText: `已批准 ${mod.approved_count} 条`,
    };
  } else if (hasPendingConfirm || mod.approved_count < mod.case_count) {
    step4 = {
      key: "confirm",
      num: 4,
      label: "确认",
      state: "active",
      stateText:
        mod.needs_human_count > 0
          ? `待确认（${mod.needs_human_count} 条需人工）`
          : "待你确认",
    };
  } else {
    step4 = {
      key: "confirm",
      num: 4,
      label: "确认",
      state: "done",
      stateText: `已批准 ${mod.approved_count} 条`,
    };
  }

  // 5. 测试
  let step5: FlowStepItem;
  if (mod.active_runs > 0) {
    step5 = {
      key: "test",
      num: 5,
      label: "测试",
      state: "active",
      stateText: `测试中（${mod.active_runs}）`,
    };
  } else if (hasAnyRun) {
    step5 = {
      key: "test",
      num: 5,
      label: "测试",
      state: "done",
      stateText: "已有执行记录",
    };
  } else if (mod.approved_count > 0) {
    step5 = {
      key: "test",
      num: 5,
      label: "测试",
      state: "pending",
      stateText: "可发起测试",
    };
  } else {
    step5 = {
      key: "test",
      num: 5,
      label: "测试",
      state: "pending",
      stateText: "未开始",
    };
  }

  return [step1, step2, step3, step4, step5];
}

function PipeIcon({ kind }: { kind: "queue" | "run" | "judge" | "done" }) {
  if (kind === "queue") {
    return (
      <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
        <circle cx="8" cy="8" r="5.5" stroke="currentColor" strokeWidth="1.5" />
        <path d="M8 5v3.2l2 1.2" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
      </svg>
    );
  }
  if (kind === "run") {
    return (
      <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
        <path
          d="M9.5 1.75L3.5 9h4.25L6.5 14.25 12.5 7H8.25L9.5 1.75z"
          stroke="currentColor"
          strokeWidth="1.4"
          strokeLinejoin="round"
        />
      </svg>
    );
  }
  if (kind === "judge") {
    return (
      <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
        <path
          d="M8 1.75l5 2.1v3.9c0 3.15-2.1 5.65-5 6.5-2.9-.85-5-3.35-5-6.5v-3.9l5-2.1z"
          stroke="currentColor"
          strokeWidth="1.4"
          strokeLinejoin="round"
        />
      </svg>
    );
  }
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="8" cy="8" r="5.5" stroke="currentColor" strokeWidth="1.5" />
      <path
        d="M5.5 8.1l1.7 1.7 3.3-3.4"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function PipeArrow({ lit }: { lit: boolean }) {
  return (
    <div className={`mod-pipe-arrow${lit ? " is-lit" : ""}`} aria-hidden="true">
      <svg width="24" height="16" viewBox="0 0 24 16" fill="none">
        <path
          d="M2 8h18"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeDasharray="3 3"
          strokeLinecap="round"
        />
        <path
          d="M15.5 4L20.5 8l-5 4"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </svg>
    </div>
  );
}

type WriteIntent =
  | { kind: "decide"; code: string; decision: "approve" | "reject"; note: string }
  | { kind: "bulk_approve" };

type ReviewFilter = "all" | ReviewStatus;

interface DecisionModalState {
  caseItem: ModuleCase;
  decision: "approve" | "reject";
  requireNote: boolean;
}

export default function ModuleDetailPage({ id }: { id: string }) {
  // 「我是谁」只记名字，在本浏览器记 30 天
  const { name: userName, setName: setUserName, expiresAt: nameExpiresAt, forget: forgetName } = useRememberedUserName();
  const trimmedName = userName.trim();

  // 轮询模块列表（找到当前 id），防闪烁缓存
  const modsCacheRef = useRef<{ sig: string; data: ModuleView[] } | null>(null);
  const loadModules = useCallback(async (signal: AbortSignal): Promise<ModuleView[]> => {
    const next = await fetchModules(project, signal);
    const sig = JSON.stringify(next);
    if (modsCacheRef.current && modsCacheRef.current.sig === sig) {
      return modsCacheRef.current.data;
    }
    modsCacheRef.current = { sig, data: next };
    return next;
  }, []);
  const {
    data: modules,
    error: modulesError,
    reload: reloadModules,
  } = usePolling(loadModules, REFRESH_MS);

  // 轮询当前模块用例列表，按 id 缓存防闪烁
  const casesCacheRef = useRef<{ id: string; sig: string; data: ModuleCase[] } | null>(null);
  const loadCases = useCallback(
    async (signal: AbortSignal): Promise<ModuleCase[]> => {
      const next = await fetchModuleCases(id, signal);
      const sig = JSON.stringify(next);
      if (
        casesCacheRef.current &&
        casesCacheRef.current.id === id &&
        casesCacheRef.current.sig === sig
      ) {
        return casesCacheRef.current.data;
      }
      casesCacheRef.current = { id, sig, data: next };
      return next;
    },
    [id],
  );
  const {
    data: casesData,
    error: casesError,
    reload: reloadCases,
  } = usePolling(loadCases, REFRESH_MS);

  const currentModule = useMemo(
    () => modules?.find((m) => m.id === id) ?? null,
    [modules, id],
  );
  const cases = casesData ?? [];

  // UI 交互状态
  const [expandedCodes, setExpandedCodes] = useState<Record<string, boolean>>({});
  const [filter, setFilter] = useState<ReviewFilter>("all");
  const [reviewPanelExpanded, setReviewPanelExpanded] = useState(false);
  const [taskDialogOpen, setTaskDialogOpen] = useState(false);

  // 批准（needs_human 必填备注）/ 拒绝（可选备注）弹窗
  const [decisionModal, setDecisionModal] = useState<DecisionModalState | null>(null);
  const [modalNote, setModalNote] = useState("");
  const [modalNoteError, setModalNoteError] = useState<string | null>(null);

  // 写操作与审核口令流程
  const submitLock = useRef(false);
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [pendingWrite, setPendingWrite] = useState<WriteIntent | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [actionNotice, setActionNotice] = useState<string | null>(null);

  const busy = busyKey !== null;

  // 切换模块 id 时清空局部展开和提示状态
  useEffect(() => {
    setExpandedCodes({});
    setFilter("all");
    setDecisionModal(null);
    setModalNote("");
    setModalNoteError(null);
    setPendingWrite(null);
    setActionError(null);
    setActionNotice(null);
  }, [id]);

  // 复核统计
  const reviewCounts = useMemo(() => {
    const counts: Record<ReviewStatus, number> = {
      draft: 0,
      reviewed_ok: 0,
      needs_human: 0,
      approved: 0,
      rejected: 0,
    };
    for (const item of cases) {
      counts[item.review_status] += 1;
    }
    return counts;
  }, [cases]);

  const approvedCodes = useMemo(
    () => cases.filter((c) => c.review_status === "approved").map((c) => c.code),
    [cases],
  );

  const filteredCases = useMemo(
    () => (filter === "all" ? cases : cases.filter((c) => c.review_status === filter)),
    [cases, filter],
  );

  // 测试中指挥台 KPI 与管线统计（全部来自各用例 latest_run.final_result / latest_run.status）
  const runTelemetry = useMemo(() => {
    let passed = 0;
    let failed = 0;
    let uncertain = 0;
    let blocked = 0;
    let inProgress = 0;
    let cancelled = 0;

    let stageQueued = 0;
    let stageRunning = 0;
    let stageJudging = 0;
    let stageDone = 0;

    let withRunCount = 0;

    for (const item of cases) {
      const run = item.latest_run;
      if (!run) continue;
      withRunCount += 1;

      if (run.status === "queued" || run.status === "assigned") {
        stageQueued += 1;
        inProgress += 1;
      } else if (run.status === "running") {
        stageRunning += 1;
        inProgress += 1;
      } else if (run.status === "judging") {
        stageJudging += 1;
        inProgress += 1;
      } else {
        stageDone += 1;
        if (run.final_result === "passed") {
          passed += 1;
        } else if (run.final_result === "failed") {
          failed += 1;
        } else if (run.final_result === "uncertain") {
          uncertain += 1;
        } else if (run.final_result === "blocked" || run.status === "blocked" || run.status === "node_lost") {
          blocked += 1;
        } else if (run.status === "cancelled") {
          cancelled += 1;
        } else if (run.status === "finished") {
          uncertain += 1;
        }
      }
    }

    const totalRuns = withRunCount > 0 ? withRunCount : cases.length;
    const completedRuns = stageDone;

    return {
      totalRuns,
      completedRuns,
      passed,
      failed,
      uncertain,
      blocked,
      inProgress,
      cancelled,
      stageQueued,
      stageRunning,
      stageJudging,
      stageDone,
    };
  }, [cases]);

  function reloadAll() {
    reloadModules();
    reloadCases();
  }

  function toggleExpand(code: string) {
    setExpandedCodes((prev) => ({ ...prev, [code]: !prev[code] }));
  }

  async function executeWrite(intent: WriteIntent, enteredToken?: string) {
    if (submitLock.current || (pendingWrite && enteredToken === undefined)) return;
    if (!trimmedName) {
      setActionError("请先填「我是谁」（只有发起该模块生成请求的人才能批准或拒绝）");
      return;
    }

    const token = enteredToken ?? readReviewToken();
    const step = writeTokenAction(!busy, token);
    if (step === "blocked") {
      setPendingWrite(null);
      return;
    }
    setActionError(null);
    if (step === "prompt") {
      setPendingWrite(intent);
      return;
    }
    if (enteredToken !== undefined) rememberReviewToken(enteredToken);

    submitLock.current = true;
    setPendingWrite(null);
    setBusyKey(intent.kind === "bulk_approve" ? "bulk_approve" : `${intent.decision}:${intent.code}`);
    setActionNotice(null);

    try {
      if (intent.kind === "bulk_approve") {
        const count = await approveReviewed(id, token, trimmedName);
        setActionNotice(`已一键批准 ${count} 条评审员认为没问题的用例。`);
      } else {
        await decideCase(id, intent.code, intent.decision, intent.note, token, trimmedName);
        setActionNotice(
          intent.decision === "approve"
            ? `已批准用例 ${intent.code}。`
            : `已拒绝用例 ${intent.code}。`,
        );
      }
    } catch (err) {
      // 服务端 403 或其他中文原因原样展示
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      submitLock.current = false;
      setBusyKey(null);
      reloadAll();
    }
  }

  function handleApproveClick(caseItem: ModuleCase) {
    setActionError(null);
    setActionNotice(null);
    if (!trimmedName) {
      setActionError("请先填「我是谁」再批准用例");
      return;
    }
    if (caseItem.review_status === "needs_human") {
      setDecisionModal({ caseItem, decision: "approve", requireNote: true });
      setModalNote("");
      setModalNoteError(null);
      return;
    }
    void executeWrite({ kind: "decide", code: caseItem.code, decision: "approve", note: "" });
  }

  function handleRejectClick(caseItem: ModuleCase) {
    setActionError(null);
    setActionNotice(null);
    if (!trimmedName) {
      setActionError("请先填「我是谁」再拒绝用例");
      return;
    }
    setDecisionModal({ caseItem, decision: "reject", requireNote: false });
    setModalNote("");
    setModalNoteError(null);
  }

  function handleConfirmDecisionModal() {
    if (!decisionModal) return;
    const note = modalNote.trim();
    if (decisionModal.requireNote && !note) {
      setModalNoteError("批准「需要人工确认」的用例时必须填写备注，说明为什么可以执行");
      return;
    }
    const target = decisionModal;
    setDecisionModal(null);
    setModalNote("");
    setModalNoteError(null);
    void executeWrite({
      kind: "decide",
      code: target.caseItem.code,
      decision: target.decision,
      note,
    });
  }

  function handleBulkApprove() {
    setActionError(null);
    setActionNotice(null);
    if (!trimmedName) {
      setActionError("请先填「我是谁」再一键批准");
      return;
    }
    if (reviewCounts.reviewed_ok === 0) return;
    void executeWrite({ kind: "bulk_approve" });
  }

  // ---------------------------------------------------------------- 加载中 / 未找到模块

  if (!modules && !modulesError) {
    return (
      <div className="mod-page">
        <section className="mod-panel" aria-label="正在加载模块详情">
          <div style={{ padding: 20 }}>
            <Skeleton active paragraph={{ rows: 6 }} />
          </div>
        </section>
      </div>
    );
  }

  if (modules && !currentModule) {
    return (
      <div className="mod-page">
        <section className="mod-empty-card" aria-label="没有这个模块">
          <Empty description="没有这个模块" />
          <a className="mod-link" href="#/modules">
            ← 返回模块列表
          </a>
        </section>
      </div>
    );
  }

  if (!currentModule) {
    return (
      <div className="mod-page">
        {modulesError && (
          <LoadError title="无法加载模块信息" error={modulesError} interval={REFRESH_MS} />
        )}
        <a className="mod-link" href="#/modules">
          ← 返回模块列表
        </a>
      </div>
    );
  }

  const isTesting = currentModule.active_runs > 0;
  const flowSteps = buildFlowSteps(currentModule, cases);

  // 共用的「我是谁」输入块
  const tokenBar = (
    <div className="mod-token-bar">
      <div className="mod-token-field">
        <label className="mod-token-label" htmlFor="mod-detail-user-token">
          我是谁
        </label>
        <Input
          id="mod-detail-user-token"
          aria-label="我是谁"
          autoComplete="off"
          maxLength={40}
          allowClear
          disabled={busy}
          value={userName}
          placeholder="你的名字（批准或拒绝用例要用）"
          onChange={(event) => {
            setUserName(event.target.value);
            if (actionError) setActionError(null);
          }}
        />
      </div>
      <div className="mod-token-note">
        <IdentityHint expiresAt={nameExpiresAt} onForget={forgetName} onCreated={setUserName} />{" "}
        数据大家都能看；批准或拒绝用例只有发起这个模块生成请求的人能做。
      </div>
    </div>
  );

  // 共用的用例复核列表（普通布局直接渲染，指挥台布局放在下方可折叠区确保不丢失功能）
  const reviewContent = (
    <>
      <div className="mod-review-toolbar">
        <div className="mod-filter-tabs" role="tablist" aria-label="按复核状态筛选用例">
          {(
            [
              { key: "all", label: `全部 (${cases.length})` },
              { key: "needs_human", label: `需要人工确认 (${reviewCounts.needs_human})` },
              { key: "reviewed_ok", label: `评审员认为没问题 (${reviewCounts.reviewed_ok})` },
              { key: "approved", label: `已批准 (${reviewCounts.approved})` },
              { key: "rejected", label: `已拒绝 (${reviewCounts.rejected})` },
              { key: "draft", label: `草稿 (${reviewCounts.draft})` },
            ] as { key: ReviewFilter; label: string }[]
          ).map((tab) => (
            <button
              key={tab.key}
              type="button"
              role="tab"
              aria-selected={filter === tab.key}
              className={`mod-filter-btn${filter === tab.key ? " is-active" : ""}`}
              onClick={() => setFilter(tab.key)}
            >
              {tab.label}
            </button>
          ))}
        </div>

        <div className="mod-bulk-wrap">
          <Button
            type="primary"
            size="small"
            disabled={busy || reviewCounts.reviewed_ok === 0}
            loading={busyKey === "bulk_approve"}
            onClick={handleBulkApprove}
          >
            一键批准评审员认为没问题的（将批准 {reviewCounts.reviewed_ok} 条）
          </Button>
          <span className="mod-bulk-note">需要人工确认的不在内，必须逐条处理</span>
        </div>
      </div>

      {!casesData && !casesError ? (
        <div style={{ padding: 20 }}>
          <Skeleton active paragraph={{ rows: 4 }} />
        </div>
      ) : filteredCases.length === 0 ? (
        <div style={{ padding: 32 }}>
          <Empty
            description={
              cases.length === 0 ? "当前模块还没有生成的用例" : "当前筛选条件下没有用例"
            }
          />
        </div>
      ) : (
        <div className="mod-case-list">
          {filteredCases.map((item) => {
            const reviewMeta = REVIEW_META[item.review_status];
            const isExpanded = Boolean(expandedCodes[item.code]);
            const isNeedsHuman = item.review_status === "needs_human";
            const runDisplay = item.latest_run ? getCaseRunDisplay(item.latest_run) : null;
            const blockedText = item.latest_run
              ? formatBlockedReason(item.latest_run.blocked_reason)
              : null;

            return (
              <article
                key={item.code}
                className={`mod-case-row${isNeedsHuman ? " is-needs-human" : ""}`}
              >
                <div className="mod-case-row-main">
                  <div className="mod-case-row-info">
                    <div className="mod-case-row-head">
                      <span className="mod-case-code">{item.code}</span>
                      <span className="mod-case-title">{item.title}</span>
                      <span className={`mod-badge mod-tone-${reviewMeta.tone}`}>
                        <span className="mod-badge-dot" aria-hidden="true" />
                        {reviewMeta.text}
                      </span>
                      <span className="mod-origin-tag">
                        {item.origin === "generated" ? "AI 生成" : "导入用例"}
                      </span>
                    </div>

                    {item.review_note ? (
                      <div
                        className={`mod-case-note mod-wrap-text${isNeedsHuman ? " is-human-note" : ""}`}
                      >
                        <span className="mod-case-note-label">评审意见 / 备注：</span>
                        {item.review_note}
                      </div>
                    ) : (
                      <div style={{ fontSize: 12, color: "var(--text-muted)" }}>
                        暂无评审意见或备注
                      </div>
                    )}
                  </div>

                  <div className="mod-case-row-actions">
                    <Button size="small" onClick={() => toggleExpand(item.code)}>
                      {isExpanded ? "收起步骤" : "展开步骤与预期"}
                    </Button>
                    <Button
                      size="small"
                      type={isNeedsHuman ? "default" : "primary"}
                      disabled={busy || item.review_status === "approved"}
                      loading={busyKey === `approve:${item.code}`}
                      onClick={() => handleApproveClick(item)}
                    >
                      {item.review_status === "approved"
                        ? "已批准"
                        : isNeedsHuman
                          ? "填备注并批准"
                          : "批准"}
                    </Button>
                    <Button
                      size="small"
                      danger
                      disabled={busy || item.review_status === "rejected"}
                      loading={busyKey === `reject:${item.code}`}
                      onClick={() => handleRejectClick(item)}
                    >
                      {item.review_status === "rejected" ? "已拒绝" : "拒绝"}
                    </Button>
                  </div>
                </div>

                {isExpanded && (
                  <div className="mod-case-expand-box">
                    <div className="mod-case-spec-col">
                      <div className="mod-case-spec-title">操作步骤（{item.steps.length}）</div>
                      {item.steps.length > 0 ? (
                        <ol className="mod-case-spec-list">
                          {item.steps.map((step, idx) => (
                            <li key={idx} className="mod-wrap-text">
                              {step}
                            </li>
                          ))}
                        </ol>
                      ) : (
                        <div className="mod-case-spec-empty">无步骤说明</div>
                      )}
                    </div>

                    <div className="mod-case-spec-col">
                      <div className="mod-case-spec-title">预期结果（{item.expected.length}）</div>
                      {item.expected.length > 0 ? (
                        <ol className="mod-case-spec-list">
                          {item.expected.map((exp, idx) => (
                            <li key={idx} className="mod-wrap-text">
                              {exp}
                            </li>
                          ))}
                        </ol>
                      ) : (
                        <div className="mod-case-spec-empty">无预期结果说明</div>
                      )}
                    </div>

                    <div className="mod-case-run-meta">
                      <span>最近执行：</span>
                      {item.latest_run && runDisplay ? (
                        <>
                          <span className={`mod-badge mod-tone-${runDisplay.tone}`}>
                            <span
                              className={`mod-badge-dot${runDisplay.pulse ? " mod-pulse-dot" : ""}`}
                              aria-hidden="true"
                            />
                            {runDisplay.text}
                          </span>
                          {blockedText && <span>受阻原因：{blockedText}</span>}
                          <span className="mod-mono">{formatDate(item.latest_run.queued_at)}</span>
                          <a
                            className="mod-link"
                            href={`#/runs/${encodeURIComponent(item.latest_run.run_id)}`}
                          >
                            查看运行记录 →
                          </a>
                        </>
                      ) : (
                        <span style={{ color: "var(--text-muted)" }}>还没有执行记录</span>
                      )}
                    </div>
                  </div>
                )}
              </article>
            );
          })}
        </div>
      )}
    </>
  );

  const stepperStrip = (
    <div className="mod-stepper" aria-label="模块处理进度">
      {flowSteps.map((step) => (
        <div
          key={step.key}
          className={`mod-step${
            step.state === "done"
              ? " is-done"
              : step.state === "active"
                ? " is-active"
                : step.state === "error"
                  ? " is-error"
                  : ""
          }`}
        >
          <div className="mod-step-top">
            <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
              <span className="mod-step-num">{step.num}</span>
              <span className="mod-step-name">{step.label}</span>
            </div>
            <span className="mod-step-state">{step.stateText}</span>
          </div>
          {step.errorDetail && (
            <div className="mod-step-detail mod-wrap-text">{step.errorDetail}</div>
          )}
        </div>
      ))}
    </div>
  );

  const alertsBlock = (modulesError || casesError || actionError || actionNotice) && (
    <div className="mod-alerts-stack">
      {modulesError && (
        <LoadError title="无法刷新模块状态" error={modulesError} interval={REFRESH_MS} />
      )}
      {casesError && (
        <LoadError title="无法加载模块用例" error={casesError} interval={REFRESH_MS} />
      )}
      {actionError && (
        <Alert
          type="error"
          showIcon
          closable
          onClose={() => setActionError(null)}
          title="操作未能完成"
          description={<div className="mod-wrap-text">{actionError}</div>}
        />
      )}
      {actionNotice && (
        <Alert
          type="info"
          showIcon
          closable
          onClose={() => setActionNotice(null)}
          title={actionNotice}
        />
      )}
    </div>
  );

  return (
    <div className="mod-page">
      {isTesting ? (
        /* ========================= 测试中：指挥台布局 ========================= */
        <>
          <header className="mod-cmd-hero" aria-label="模块测试指挥台">
            <div className="mod-cmd-hero-top">
              <div className="mod-cmd-brand">
                <div className="mod-cmd-emblem" aria-hidden="true">
                  &gt;_
                </div>
                <div className="mod-cmd-titles">
                  <a className="mod-back-link" href="#/modules">
                    ← 返回模块列表
                  </a>
                  <div className="mod-cmd-heading-row">
                    <h1 className="mod-cmd-title">{currentModule.name}</h1>
                    <span className="mod-cmd-scope-tag">测试指挥台</span>
                  </div>
                  <div className="mod-cmd-runtime">
                    <span className="mod-badge mod-tone-run">
                      <span className="mod-badge-dot mod-pulse-dot" aria-hidden="true" />
                      测试中
                    </span>
                    <span aria-hidden="true">·</span>
                    <span className="mod-mono">{currentModule.active_runs} 条运行中</span>
                    <span aria-hidden="true">·</span>
                    <span className="mod-mono">自动轮询 {REFRESH_MS / 1000}s</span>
                  </div>
                </div>
              </div>

              {/* 中央超大 KPI：已完成 N / 总 M */}
              <div className="mod-cmd-kpi-center" aria-label="测试完成进度">
                <div className="mod-cmd-kpi-row">
                  <span className="mod-cmd-kpi-prefix">已完成</span>
                  <span className="mod-cmd-kpi-big">{runTelemetry.completedRuns}</span>
                  <span className="mod-cmd-kpi-denom">/ 总 {runTelemetry.totalRuns}</span>
                </div>
                <div className="mod-cmd-kpi-caption">
                  模块共 {cases.length} 条用例 · 已批准 {approvedCodes.length} 条
                </div>
              </div>

              {/* 右侧副 KPI：通过 / 失败 / 待人工确认 / 受阻 / 进行中 */}
              <div className="mod-cmd-side-kpis" aria-label="测试结果分布">
                <div className="mod-cmd-side-item">
                  <span className="mod-cmd-side-label">通过</span>
                  <span
                    className={`mod-cmd-side-val${runTelemetry.passed > 0 ? " is-pass" : " is-muted"}`}
                  >
                    {runTelemetry.passed}
                  </span>
                </div>
                <div className="mod-cmd-side-item">
                  <span className="mod-cmd-side-label">失败</span>
                  <span
                    className={`mod-cmd-side-val${runTelemetry.failed > 0 ? " is-fail" : " is-muted"}`}
                  >
                    {runTelemetry.failed}
                  </span>
                </div>
                <div className="mod-cmd-side-item">
                  <span className="mod-cmd-side-label">待人工确认</span>
                  <span
                    className={`mod-cmd-side-val${runTelemetry.uncertain > 0 ? " is-unc" : " is-muted"}`}
                  >
                    {runTelemetry.uncertain}
                  </span>
                </div>
                <div className="mod-cmd-side-item">
                  <span className="mod-cmd-side-label">受阻</span>
                  <span
                    className={`mod-cmd-side-val${runTelemetry.blocked > 0 ? " is-blk" : " is-muted"}`}
                  >
                    {runTelemetry.blocked}
                  </span>
                </div>
                <div className="mod-cmd-side-item">
                  <span className="mod-cmd-side-label">进行中</span>
                  <span
                    className={`mod-cmd-side-val${runTelemetry.inProgress > 0 ? " is-run" : " is-muted"}`}
                  >
                    {runTelemetry.inProgress}
                  </span>
                </div>
              </div>
            </div>

            {stepperStrip}
          </header>

          {alertsBlock}

          {/* 指挥台左右两栏（<900px 自动上下排列） */}
          <div className="mod-cmd-workspace">
            {/* 左栏：用例实体卡纵向堆叠 */}
            <aside className="mod-cmd-sidebar" aria-label="用例实时状态">
              <div className="mod-cmd-sidebar-head">
                <span className="mod-section-kicker">用例实体卡</span>
                <span className="mod-mono" style={{ fontSize: 12, color: "var(--text-muted)" }}>
                  {cases.length} 条用例
                </span>
              </div>

              <div className="mod-cmd-card-stack">
                {cases.map((item) => {
                  const display = getCaseRunDisplay(item.latest_run);
                  const rawStatusMeta = item.latest_run
                    ? getRunStatusMeta(item.latest_run.status)
                    : null;
                  const showFinalAndStatus =
                    item.latest_run &&
                    item.latest_run.final_result &&
                    rawStatusMeta &&
                    rawStatusMeta.text !== display.text;

                  return (
                    <div
                      key={item.code}
                      className={`mod-entity-card${display.cardClass}`}
                    >
                      <div className="mod-entity-card-top">
                        <span className="mod-case-code">{item.code}</span>
                        <span className={`mod-badge mod-tone-${display.tone}`}>
                          <span
                            className={`mod-badge-dot${display.pulse ? " mod-pulse-dot" : ""}`}
                            aria-hidden="true"
                          />
                          {display.text}
                        </span>
                      </div>

                      <div className="mod-entity-title">{item.title}</div>

                      <div className="mod-entity-foot">
                        <span>
                          {showFinalAndStatus && rawStatusMeta
                            ? `状态：${rawStatusMeta.text}`
                            : item.latest_run
                              ? `排队：${formatTime(item.latest_run.queued_at)}`
                              : "还没有执行记录"}
                        </span>
                        {item.latest_run && (
                          <a
                            className="mod-link"
                            href={`#/runs/${encodeURIComponent(item.latest_run.run_id)}`}
                          >
                            运行详情 →
                          </a>
                        )}
                      </div>

                      <div className="mod-entity-meter" aria-hidden="true">
                        <div className={`mod-entity-meter-fill ${display.meterClass}`} />
                      </div>
                    </div>
                  );
                })}
              </div>
            </aside>

            {/* 右栏：流程管线 + 统计块 + 类终端运行事件 */}
            <main className="mod-cmd-main">
              <section className="mod-panel" aria-label="测试流程管线">
                <div className="mod-panel-head">
                  <div className="mod-panel-title-wrap">
                    <h2 className="mod-panel-title">执行管线 · 排队 → 执行中 → 判定中 → 完成</h2>
                    <span className="mod-panel-sub">实时统计各阶段用例数</span>
                  </div>
                  <Button
                    type="primary"
                    size="small"
                    disabled={approvedCodes.length === 0}
                    onClick={() => setTaskDialogOpen(true)}
                  >
                    对已批准的用例发起测试（{approvedCodes.length}）
                  </Button>
                </div>

                <div className="mod-pipe-body">
                  <div className="mod-pipe-track" role="group" aria-label="执行阶段分布">
                    <div
                      className={`mod-pipe-node${runTelemetry.stageQueued > 0 ? " is-lit" : ""}`}
                    >
                      <div className="mod-pipe-icon">
                        <PipeIcon kind="queue" />
                      </div>
                      <span className="mod-pipe-stage-name">排队</span>
                      <span className="mod-pipe-stage-count">{runTelemetry.stageQueued}</span>
                    </div>

                    <PipeArrow lit={runTelemetry.stageRunning > 0} />

                    <div
                      className={`mod-pipe-node${runTelemetry.stageRunning > 0 ? " is-lit" : ""}`}
                    >
                      <div className="mod-pipe-icon">
                        <PipeIcon kind="run" />
                      </div>
                      <span className="mod-pipe-stage-name">执行中</span>
                      <span className="mod-pipe-stage-count">{runTelemetry.stageRunning}</span>
                    </div>

                    <PipeArrow lit={runTelemetry.stageJudging > 0} />

                    <div
                      className={`mod-pipe-node${runTelemetry.stageJudging > 0 ? " is-lit" : ""}`}
                    >
                      <div className="mod-pipe-icon">
                        <PipeIcon kind="judge" />
                      </div>
                      <span className="mod-pipe-stage-name">判定中</span>
                      <span className="mod-pipe-stage-count">{runTelemetry.stageJudging}</span>
                    </div>

                    <PipeArrow lit={runTelemetry.stageDone > 0} />

                    <div
                      className={`mod-pipe-node${runTelemetry.stageDone > 0 ? " is-done-lit" : ""}`}
                    >
                      <div className="mod-pipe-icon">
                        <PipeIcon kind="done" />
                      </div>
                      <span className="mod-pipe-stage-name">完成</span>
                      <span className="mod-pipe-stage-count">{runTelemetry.stageDone}</span>
                    </div>
                  </div>

                  <div className="mod-cmd-stats-grid">
                    <div className="mod-stat-tile">
                      <span className="mod-stat-label">通过</span>
                      <span
                        className={`mod-stat-num${runTelemetry.passed > 0 ? " is-pass" : ""}`}
                      >
                        {runTelemetry.passed}
                      </span>
                    </div>
                    <div className={`mod-stat-tile${runTelemetry.failed > 0 ? " is-alert" : ""}`}>
                      <span className="mod-stat-label">失败</span>
                      <span
                        className={`mod-stat-num${runTelemetry.failed > 0 ? " is-fail" : ""}`}
                      >
                        {runTelemetry.failed}
                      </span>
                    </div>
                    <div className={`mod-stat-tile${runTelemetry.uncertain > 0 ? " is-warn" : ""}`}>
                      <span className="mod-stat-label">待人工确认</span>
                      <span
                        className={`mod-stat-num${runTelemetry.uncertain > 0 ? " is-unc" : ""}`}
                      >
                        {runTelemetry.uncertain}
                      </span>
                    </div>
                    <div className="mod-stat-tile">
                      <span className="mod-stat-label">受阻</span>
                      <span className="mod-stat-num">{runTelemetry.blocked}</span>
                    </div>
                  </div>
                </div>
              </section>

              {/* 类终端运行事件面板：只展示每条用例真实的 latest_run，不编造日志 */}
              <section className="mod-terminal" aria-label="运行事件">
                <div className="mod-terminal-bar">
                  <div className="mod-terminal-cmd">
                    <span>$ run-events.log</span>
                    <span className="mod-terminal-title">运行事件（各用例最近一次执行）</span>
                  </div>
                  <span className="mod-terminal-count">{cases.length} 条用例</span>
                </div>

                <div className="mod-terminal-body">
                  {cases.length === 0 ? (
                    <div className="mod-terminal-empty">还没有执行记录</div>
                  ) : (
                    cases.map((item) => {
                      const run = item.latest_run;
                      if (!run) {
                        return (
                          <div key={item.code} className="mod-term-row">
                            <div className="mod-term-row-left">
                              <span className="mod-term-code">{item.code}</span>
                              <span className="mod-term-case-title">{item.title}</span>
                              <span className="mod-term-none">还没有执行记录</span>
                            </div>
                          </div>
                        );
                      }

                      const statusMeta = getRunStatusMeta(run.status);
                      const finalMeta = run.final_result
                        ? FINAL_RESULT_META[run.final_result]
                        : null;
                      const blockedText = formatBlockedReason(run.blocked_reason);

                      return (
                        <div key={item.code} className="mod-term-row">
                          <div className="mod-term-row-left">
                            <span className="mod-term-time">{formatTime(run.queued_at)}</span>
                            <span className="mod-term-code">{item.code}</span>
                            <span className="mod-term-case-title">{item.title}</span>
                            <span className={`mod-badge mod-tone-${statusMeta.tone}`}>
                              <span
                                className={`mod-badge-dot${isRunActive(run) && (run.status === "running" || run.status === "judging") ? " mod-pulse-dot" : ""}`}
                                aria-hidden="true"
                              />
                              {statusMeta.text}
                            </span>
                            {finalMeta && (
                              <span className={`mod-badge mod-tone-${finalMeta.tone}`}>
                                <span className="mod-badge-dot" aria-hidden="true" />
                                结果：{finalMeta.text}
                              </span>
                            )}
                            {blockedText && (
                              <span className="mod-term-reason">受阻：{blockedText}</span>
                            )}
                          </div>
                          <a
                            className="mod-term-run-link"
                            href={`#/runs/${encodeURIComponent(run.run_id)}`}
                          >
                            #/runs/{run.run_id.slice(0, 8)} →
                          </a>
                        </div>
                      );
                    })
                  )}
                </div>
              </section>
            </main>
          </div>

          {/* 指挥台布局下依然可访问完整的用例复核列表 */}
          <section className="mod-panel" aria-label="用例复核折叠区">
            <button
              type="button"
              className="mod-collapse-head"
              aria-expanded={reviewPanelExpanded}
              onClick={() => setReviewPanelExpanded((prev) => !prev)}
            >
              <div className="mod-panel-title-wrap">
                <h2 className="mod-panel-title">用例复核与批准</h2>
                <span className="mod-panel-sub">
                  共 {cases.length} 条 · 已批准 {reviewCounts.approved} 条
                </span>
                {reviewCounts.needs_human > 0 && (
                  <span className="mod-badge mod-tone-unc">
                    <span className="mod-badge-dot" aria-hidden="true" />
                    {reviewCounts.needs_human} 条需要人工确认
                  </span>
                )}
              </div>
              <span className="mod-collapse-toggle">
                {reviewPanelExpanded ? "收起复核列表 ▲" : "展开复核列表 ▼"}
              </span>
            </button>

            {reviewPanelExpanded && (
              <div style={{ borderTop: "1px solid var(--border-hairline)" }}>
                <div style={{ padding: "12px 20px", borderBottom: "1px solid var(--border-hairline)" }}>
                  {tokenBar}
                </div>
                {reviewContent}
              </div>
            )}
          </section>
        </>
      ) : (
        /* ========================= 非测试中：简洁普通布局 ========================= */
        <>
          <header className="mod-detail-header">
            <div className="mod-detail-header-top">
              <div className="mod-detail-title-col">
                <a className="mod-back-link" href="#/modules">
                  ← 返回模块列表
                </a>
                <div className="mod-detail-title-row">
                  <h1 className="mod-detail-title">{currentModule.name}</h1>
                  {currentModule.latest_request?.status === "failed" ? (
                    <span className="mod-badge mod-tone-fail">
                      <span className="mod-badge-dot" aria-hidden="true" />
                      生成失败
                    </span>
                  ) : currentModule.latest_request?.status === "queued" ||
                    currentModule.latest_request?.status === "running" ? (
                    <span className="mod-badge mod-tone-run">
                      <span className="mod-badge-dot mod-pulse-dot" aria-hidden="true" />
                      生成中
                    </span>
                  ) : currentModule.approved_count > 0 &&
                    currentModule.approved_count === currentModule.case_count ? (
                    <span className="mod-badge mod-tone-pass">
                      <span className="mod-badge-dot" aria-hidden="true" />
                      可以发起测试
                    </span>
                  ) : currentModule.case_count > 0 ? (
                    <span
                      className={`mod-badge ${
                        currentModule.needs_human_count > 0 ? "mod-tone-unc" : "mod-tone-que"
                      }`}
                    >
                      <span className="mod-badge-dot" aria-hidden="true" />
                      待复核确认
                    </span>
                  ) : (
                    <span className="mod-badge mod-tone-que">
                      <span className="mod-badge-dot" aria-hidden="true" />
                      暂无用例
                    </span>
                  )}
                </div>
                <div className="mod-detail-sub">
                  <span className="mod-mono">共 {currentModule.case_count} 条用例</span>
                  <span aria-hidden="true">·</span>
                  <span className="mod-mono">
                    已批准 {casesData ? approvedCodes.length : currentModule.approved_count} 条
                  </span>
                  <span aria-hidden="true">·</span>
                  <span className="mod-mono">待人工确认 {currentModule.needs_human_count} 条</span>
                  <span aria-hidden="true">·</span>
                  <span className="mod-mono">创建于 {formatDate(currentModule.created_at)}</span>
                </div>
              </div>

              <div className="mod-detail-actions">
                <Button
                  type="primary"
                  disabled={approvedCodes.length === 0}
                  onClick={() => setTaskDialogOpen(true)}
                >
                  对已批准的用例发起测试（{approvedCodes.length}）
                </Button>
              </div>
            </div>

            {stepperStrip}
          </header>

          {alertsBlock}

          {tokenBar}

          <section className="mod-panel" aria-label="用例复核列表">
            <div className="mod-panel-head">
              <div className="mod-panel-title-wrap">
                <h2 className="mod-panel-title">用例复核列表</h2>
                <span className="mod-panel-sub">
                  评审员认为没问题的用例仍需你确认批准后才能发起测试
                </span>
              </div>
            </div>
            {reviewContent}
          </section>
        </>
      )}

      {/* 批准（needs_human 必填备注）/ 拒绝（可选备注）弹窗 */}
      {decisionModal && (
        <Modal
          open
          title={
            decisionModal.decision === "approve"
              ? `批准用例（需填备注）：${decisionModal.caseItem.code}`
              : `拒绝用例：${decisionModal.caseItem.code}`
          }
          okText={decisionModal.decision === "approve" ? "确认批准" : "确认拒绝"}
          cancelText="取消"
          okButtonProps={{
            danger: decisionModal.decision === "reject",
            disabled: busy,
          }}
          cancelButtonProps={{ disabled: busy }}
          mask={{ closable: !busy }}
          keyboard={!busy}
          closable={!busy}
          onOk={handleConfirmDecisionModal}
          onCancel={() => {
            setDecisionModal(null);
            setModalNote("");
            setModalNoteError(null);
          }}
        >
          <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
            <div style={{ fontSize: 13, color: "var(--text-primary)", fontWeight: 600 }}>
              {decisionModal.caseItem.title}
            </div>
            {decisionModal.requireNote ? (
              <Alert
                type="warning"
                showIcon
                title="该用例标记为「需要人工确认」"
                description="批准前必须填写备注，说明为什么可以执行。"
              />
            ) : (
              <div style={{ fontSize: 12, color: "var(--text-secondary)" }}>
                备注为可选项，可填写拒绝原因或后续修改建议。
              </div>
            )}
            <Input.TextArea
              rows={4}
              value={modalNote}
              placeholder={
                decisionModal.requireNote
                  ? "必填：说明人工核对后的判断及为何可以执行"
                  : "可选：填写拒绝原因"
              }
              onChange={(event) => {
                setModalNote(event.target.value);
                if (modalNoteError) setModalNoteError(null);
              }}
            />
            {modalNoteError && <div className="mod-field-error">{modalNoteError}</div>}
          </div>
        </Modal>
      )}

      {/* 发起测试弹窗（传入本模块已批准用例的 code 列表） */}
      {taskDialogOpen && (
        <TaskDialogWithCases
          initialCases={approvedCodes}
          onClose={() => {
            setTaskDialogOpen(false);
            reloadAll();
          }}
        />
      )}

      {/* 审核口令弹窗 */}
      {pendingWrite && (
        <ReviewTokenDialog
          disabled={busy}
          onConfirm={(token) => void executeWrite(pendingWrite, token)}
          onCancel={() => setPendingWrite(null)}
        />
      )}
    </div>
  );
}
