import { Alert, Button, Empty, Popconfirm, Segmented, Select, Skeleton, Table, Typography } from "antd";
import type { ColumnsType } from "antd/es/table";
import { useCallback, useMemo, useState } from "react";
import { cancelRun, fetchReviewStats, fetchTask, fetchTasks, type RunView, type TaskDetail, type TaskSummary } from "./api";
import NewTaskDialog from "./NewTaskDialog";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";
import { blockedLabel, FinalTag, formatDate, formatDuration, hasDisagreement, isTerminal, JudgeTag, LoadError, parseSummary, Pill, SegBar, StatusPill, VerdictTag, type VerdictCounts } from "./runUi";
import { REFRESH_MS, usePolling } from "./usePolling";

const taskColumns: ColumnsType<TaskSummary> = [
  {
    title: "创建时间", dataIndex: "created_at", width: 184,
    render: (time: string, task) => (
      <a className="task-time-link mono" href={`#/tasks/${task.task_id}`}>{formatDate(time)}</a>
    ),
  },
  {
    title: "项目", dataIndex: "project", width: 110,
    render: (project: string) => <span className="task-project-name">{project}</span>,
  },
  {
    title: "用例数", dataIndex: "run_count", width: 88,
    render: (count: number) => <span className="code-tag code-tag-muted">{count} 条</span>,
  },
  {
    title: "结果概览", width: 256,
    render: (_, task) => (
      <div>
        <SegBar counts={parseSummary(task.result_summary)} />
        <div className="task-summary-text">{task.result_summary || "—"}</div>
      </div>
    ),
  },
  {
    title: "节点", dataIndex: "node_id", width: 120,
    render: (id: string) => <span className="code-tag" title={id}>{id.slice(0, 8)}</span>,
  },
  {
    title: "模型", width: 224,
    render: (_, task) => (
      <div className="task-model-cell">
        <div>
          <span className="code-tag">
            {task.executor.model ?? "—"}{task.executor.effort && ` · ${task.executor.effort}`}
          </span>
        </div>
        <div className="task-model-sub mono">Judge: {task.judge.model ?? "—"}</div>
      </div>
    ),
  },
];

const percent = new Intl.NumberFormat("zh-CN", { style: "percent", maximumFractionDigits: 1 });

function KpiRow({ tasks }: { tasks: TaskSummary[] | null }) {
  const { data: stats, error } = usePolling(fetchReviewStats, 5000);
  const sum = (key: "running" | "uncertain") => (tasks ?? []).reduce((n, t) => n + parseSummary(t.result_summary)[key], 0);
  return (
    <section className="kpi-grid" aria-label="汇总">
      <div className="surface kpi-card">
        <div className="kpi-label">Judge 判对率</div>
        <div className="kpi-value kpi-pass">{stats ? (stats.judge_correct_rate == null ? "暂无" : percent.format(stats.judge_correct_rate)) : "—"}</div>
        <div className="kpi-note">{error ? "无法加载审核统计" : stats && stats.reviewed < 30 ? "样本不足，只作参考" : "来自人工审核"}</div>
      </div>
      <div className="surface kpi-card">
        <div className="kpi-label">人工已审核</div>
        <div className="kpi-value">{stats ? stats.reviewed : "—"}</div>
        <div className="kpi-note"><a href="/api/reviews/export">导出审核数据</a></div>
      </div>
      <div className="surface kpi-card">
        <div className="kpi-label">进行中用例</div>
        <div className="kpi-value kpi-run">{tasks ? sum("running") : "—"}</div>
        <div className="kpi-note">自动刷新</div>
      </div>
      <div className="surface kpi-card">
        <div className="kpi-label">待人工确认</div>
        <div className="kpi-value kpi-unc">{tasks ? sum("uncertain") : "—"}</div>
        <div className="kpi-note">需要人看一眼</div>
      </div>
    </section>
  );
}

type ResultFilter = "all" | "running" | "failed" | "uncertain" | "blocked";

function matches(task: TaskSummary, filter: ResultFilter) {
  if (filter === "all") return true;
  const key = filter === "running" ? "running" : filter;
  return parseSummary(task.result_summary)[key] > 0;
}

export function TasksPage() {
  const { data, error } = usePolling(fetchTasks);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [filter, setFilter] = useState<ResultFilter>("all");
  const [project, setProject] = useState("all");
  const projects = useMemo(() => [...new Set((data ?? []).map((t) => t.project))], [data]);
  // 按创建时间倒序，再按筛选条件过滤
  const rows = useMemo(
    () => [...(data ?? [])]
      .filter((t) => (project === "all" || t.project === project) && matches(t, filter))
      .sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at)),
    [data, filter, project],
  );
  return (
    <>
      <div className="page-heading">
        <div className="page-title-block">
          <Typography.Title level={3}>测试任务</Typography.Title>
          <p className="page-desc">掌握各任务通过 / 失败 / 待人工确认比例，点击任意任务查看详细用例与运行证据。</p>
        </div>
        <Button type="primary" onClick={() => setDialogOpen(true)}>发起任务</Button>
      </div>
      <KpiRow tasks={data} />
      {error ? <LoadError title="无法加载任务列表" error={error} /> : !data ? <Skeleton active /> : (
        <section className="card table-card" aria-label="任务列表">
          <div className="card-header">
            <h2 className="card-title">最近测试任务（{rows.length}）</h2>
            <div className="filter-controls">
              <Select
                aria-label="按项目筛选"
                value={project}
                onChange={setProject}
                style={{ width: 160 }}
                options={[{ value: "all", label: "全部项目" }, ...projects.map((p) => ({ value: p, label: p }))]}
              />
              <Segmented<ResultFilter>
                aria-label="按结果筛选"
                value={filter}
                onChange={setFilter}
                options={[
                  { value: "all", label: "全部" },
                  { value: "running", label: "进行中" },
                  { value: "failed", label: "含失败" },
                  { value: "uncertain", label: "待确认" },
                  { value: "blocked", label: "含受阻" },
                ]}
              />
            </div>
          </div>
          <Table<TaskSummary>
            rowKey="task_id" columns={taskColumns} dataSource={rows} pagination={false} scroll={{ x: 980 }}
            locale={{
              emptyText: (
                <div className="table-empty-box">
                  <Empty description={data.length ? "没有符合筛选条件的任务" : "还没有任务"} />
                  {data.length > 0 && (
                    <Button size="small" onClick={() => { setProject("all"); setFilter("all"); }}>重置筛选条件</Button>
                  )}
                </div>
              ),
            }}
            onRow={(task) => {
              const openTask = () => {
                window.location.hash = `/tasks/${task.task_id}`;
              };
              return {
                className: "task-row",
                tabIndex: 0,
                onClick: (event) => {
                  if (event.target instanceof Element && event.target.closest("a")) return;
                  openTask();
                },
                onKeyDown: (event) => {
                  if (event.key !== "Enter" || event.target !== event.currentTarget) return;
                  event.preventDefault();
                  openTask();
                },
              };
            }}
          />
        </section>
      )}
      {dialogOpen && <NewTaskDialog onClose={() => setDialogOpen(false)} />}
    </>
  );
}

const taskInProgress = (task: TaskDetail) => task.runs.some((run) => !isTerminal(run));

function countRuns(runs: RunView[]): VerdictCounts {
  const counts: VerdictCounts = { passed: 0, failed: 0, uncertain: 0, blocked: 0, running: 0, cancelled: 0 };
  for (const run of runs) {
    if (run.final_result === "passed") counts.passed += 1;
    else if (run.final_result === "failed") counts.failed += 1;
    else if (run.final_result === "uncertain") counts.uncertain += 1;
    else if (run.final_result === "blocked" || run.status === "blocked") counts.blocked += 1;
    else if (run.status === "cancelled") counts.cancelled += 1;
    else if (run.status === "finished") counts.uncertain += 1;
    else counts.running += 1;
  }
  return counts;
}

function summarizeCounts(counts: VerdictCounts): string {
  const parts: string[] = [];
  if (counts.passed) parts.push(`${counts.passed} 通过`);
  if (counts.failed) parts.push(`${counts.failed} 失败`);
  if (counts.uncertain) parts.push(`${counts.uncertain} 待确认`);
  if (counts.blocked) parts.push(`${counts.blocked} 受阻`);
  if (counts.running) parts.push(`${counts.running} 进行中`);
  if (counts.cancelled) parts.push(`${counts.cancelled} 已取消`);
  return parts.length ? parts.join(" · ") : "暂无运行记录";
}

export function TaskPage({ id }: { id: string }) {
  const load = useCallback((signal: AbortSignal) => fetchTask(id, signal), [id]);
  const { data: task, error, reload } = usePolling(load, REFRESH_MS, taskInProgress);
  const [actionError, setActionError] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState<string | null>(null);
  const [pendingCancel, setPendingCancel] = useState<string | null>(null);

  async function cancel(runId: string, enteredToken?: string) {
    const token = enteredToken ?? readReviewToken();
    const action = writeTokenAction(cancelling === null, token);
    if (action === "blocked") return;
    setActionError(null);
    if (action === "prompt") {
      setPendingCancel(runId);
      return;
    }
    if (enteredToken !== undefined) rememberReviewToken(enteredToken);
    setPendingCancel(null);
    setCancelling(runId);
    try {
      await cancelRun(runId, token);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setCancelling(null);
      reload();
    }
  }

  const columns: ColumnsType<RunView> = [
    {
      title: "用例", dataIndex: "code", width: 132,
      render: (code: string, run) => (
        <div>
          <a className="run-code-link" href={`#/runs/${run.run_id}`}>{code}</a>
          {hasDisagreement(run) && <div className="mismatch-badge"><Pill kind="uncertain">自评与 Judge 不一致</Pill></div>}
        </div>
      ),
    },
    {
      title: "状态", width: 152,
      render: (_, run) => (
        <div>
          <StatusPill status={run.status} />
          {run.blocked_reason && <div className="blocked-note">{blockedLabel(run.blocked_reason)}</div>}
        </div>
      ),
    },
    { title: "执行员自评", width: 136, render: (_, run) => run.executor_result ? <VerdictTag verdict={run.executor_result} /> : "—" },
    { title: "Judge", width: 148, render: (_, run) => <JudgeTag status={run.judge_status} /> },
    { title: "最终结果", width: 140, render: (_, run) => <FinalTag run={run} /> },
    { title: "设备序列号", width: 156, render: (_, run) => run.device_serial ? <span className="code-tag">{run.device_serial}</span> : "—" },
    { title: "耗时", width: 108, render: (_, run) => <span className="mono">{formatDuration(run)}</span> },
    {
      // 固定在右侧：表格横向滚动时取消按钮也不会被推出视口。
      title: "操作", width: 104, fixed: "right",
      render: (_, run) => !isTerminal(run) && (
        <Popconfirm
          title={`取消 ${run.code} 的本次执行？`} okText="确认取消" cancelText="返回"
          onConfirm={() => cancel(run.run_id)} okButtonProps={{ danger: true }}
        >
          <Button size="small" danger disabled={cancelling !== null || pendingCancel !== null} loading={cancelling === run.run_id}>取消</Button>
        </Popconfirm>
      ),
    },
  ];

  return (
    <>
      <a className="back-link" href="#/tasks">← 返回任务列表</a>
      <div className="page-heading">
        <div className="page-title-block">
          <Typography.Title level={3}>任务详情</Typography.Title>
          <p className="page-desc">查看任务配置、执行节点与各用例运行结果，点击用例编号进入完整时间线与审核页。</p>
        </div>
      </div>
      {error ? <LoadError title="无法加载任务详情" error={error} /> : !task ? <Skeleton active /> : (() => {
        const counts = countRuns(task.runs);
        return (
          <div className="page-stack">
            <section className="card task-hero" aria-label="任务概览">
              <div className="task-hero-main">
                <div className="task-hero-title">
                  <span className="task-project-name">{task.project}</span>
                  <span className="code-tag" title={task.task_id}>{task.task_id.slice(0, 8)}</span>
                </div>
                <div className="task-hero-meta">
                  <span className="task-meta-item">
                    <span className="task-meta-label">节点</span>
                    <span className="code-tag" title={task.node_id}>{task.node_id.slice(0, 8)}</span>
                  </span>
                  <span className="task-meta-item">
                    <span className="task-meta-label">执行员</span>
                    <span className="code-tag code-tag-muted">
                      {task.executor.model ?? "模型来源默认"}{task.executor.effort ? ` · ${task.executor.effort}` : ""}
                    </span>
                  </span>
                  <span className="task-meta-item">
                    <span className="task-meta-label">判定员</span>
                    <span className="code-tag code-tag-muted">
                      {[task.judge.model ?? "模型来源默认", task.judge.effort].filter(Boolean).join(" · ")}
                    </span>
                  </span>
                  <span className="task-meta-item">
                    <span className="task-meta-label">创建于</span>
                    <span className="mono">{formatDate(task.created_at)}</span>
                  </span>
                </div>
              </div>
              <div className="task-hero-summary">
                <SegBar counts={counts} width={200} />
                <div className="task-summary-text">{summarizeCounts(counts)}</div>
              </div>
            </section>
            {actionError && <Alert type="error" showIcon title={actionError} />}
            <section className="card table-card" aria-label="用例运行列表">
              <div className="card-header">
                <h2 className="card-title">用例运行列表（{task.runs.length} 条）</h2>
                <span className="card-subtitle">点击任意用例进入步骤时间线、录屏与人工审核</span>
              </div>
              <Table<RunView>
                rowKey="run_id"
                columns={columns}
                dataSource={task.runs}
                pagination={false}
                scroll={{ x: 1034 }}
                onRow={(run) => ({
                  className: "task-row",
                  onClick: (event) => {
                    if (event.target instanceof Element && event.target.closest("a, button, .ant-popover")) return;
                    window.location.hash = `/runs/${run.run_id}`;
                  },
                })}
              />
            </section>
          </div>
        );
      })()}
      {pendingCancel && (
        <ReviewTokenDialog onConfirm={(token) => cancel(pendingCancel, token)} onCancel={() => setPendingCancel(null)} />
      )}
    </>
  );
}
