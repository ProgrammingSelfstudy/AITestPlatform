import { Alert, Button, Descriptions, Empty, Popconfirm, Segmented, Select, Skeleton, Table, Typography } from "antd";
import type { ColumnsType } from "antd/es/table";
import { useCallback, useMemo, useState } from "react";
import { cancelRun, fetchReviewStats, fetchTask, fetchTasks, type RunView, type TaskDetail, type TaskSummary } from "./api";
import NewTaskDialog from "./NewTaskDialog";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";
import { blockedLabel, FinalTag, formatDate, formatDuration, hasDisagreement, isTerminal, JudgeTag, LoadError, parseSummary, Pill, SegBar, StatusPill, VerdictTag } from "./runUi";
import { REFRESH_MS, usePolling } from "./usePolling";

const taskColumns: ColumnsType<TaskSummary> = [
  {
    title: "创建时间", dataIndex: "created_at", width: 178,
    render: (time: string, task) => <a href={`#/tasks/${task.task_id}`}>{formatDate(time)}</a>,
  },
  { title: "项目", dataIndex: "project", width: 90 },
  { title: "用例数", dataIndex: "run_count", width: 84 },
  {
    title: "结果概览", width: 250,
    render: (_, task) => (
      <div>
        <SegBar counts={parseSummary(task.result_summary)} />
        <Typography.Text type="secondary">{task.result_summary || "—"}</Typography.Text>
      </div>
    ),
  },
  { title: "节点", dataIndex: "node_id", width: 105, render: (id: string) => <code title={id}>{id.slice(0, 8)}</code> },
  {
    title: "模型", width: 210,
    render: (_, task) => (
      <div>
        <div>{task.executor.model ?? "—"}{task.executor.effort && ` · ${task.executor.effort}`}</div>
        <Typography.Text type="secondary">{task.judge.model ?? "—"}</Typography.Text>
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
      <div className="surface kpi-card"><div className="kpi-label">进行中用例</div><div className="kpi-value kpi-run">{tasks ? sum("running") : "—"}</div><div className="kpi-note">自动刷新</div></div>
      <div className="surface kpi-card"><div className="kpi-label">待人工确认</div><div className="kpi-value kpi-unc">{tasks ? sum("uncertain") : "—"}</div><div className="kpi-note">需要人看一眼</div></div>
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
        <Typography.Title level={3}>任务</Typography.Title>
        <Button type="primary" onClick={() => setDialogOpen(true)}>发起任务</Button>
      </div>
      <KpiRow tasks={data} />
      {error ? <LoadError title="无法加载任务列表" error={error} /> : !data ? <Skeleton active /> : (
        <>
        <div className="filter-bar">
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
          <Select
            aria-label="按项目筛选"
            value={project}
            onChange={setProject}
            style={{ width: 160 }}
            options={[{ value: "all", label: "全部项目" }, ...projects.map((p) => ({ value: p, label: p }))]}
          />
        </div>
        <Table<TaskSummary>
          rowKey="task_id" columns={taskColumns} dataSource={rows} pagination={false} scroll={{ x: 1000 }}
          locale={{ emptyText: <Empty description={data.length ? "没有符合筛选条件的任务" : "还没有任务"} /> }}
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
        </>
      )}
      {dialogOpen && <NewTaskDialog onClose={() => setDialogOpen(false)} />}
    </>
  );
}

const taskInProgress = (task: TaskDetail) => task.runs.some((run) => !isTerminal(run));

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
    { title: "用例", dataIndex: "code", width: 116, render: (code: string, run) => (
      <div>
        <a href={`#/runs/${run.run_id}`}>{code}</a>
        {hasDisagreement(run) && <div className="mismatch-badge"><Pill kind="uncertain">自评与 Judge 不一致</Pill></div>}
      </div>
    ) },
    {
      title: "状态", width: 146,
      render: (_, run) => (
        <div>
          <StatusPill status={run.status} />
          {run.blocked_reason && <div><Typography.Text type="secondary">{blockedLabel(run.blocked_reason)}</Typography.Text></div>}
        </div>
      ),
    },
    { title: "执行员自评", width: 132, render: (_, run) => run.executor_result ? <VerdictTag verdict={run.executor_result} /> : "—" },
    { title: "Judge", width: 146, render: (_, run) => <JudgeTag status={run.judge_status} /> },
    { title: "最终结果", width: 138, render: (_, run) => <FinalTag run={run} /> },
    { title: "设备序列号", width: 150, render: (_, run) => run.device_serial ? <code>{run.device_serial}</code> : "—" },
    { title: "耗时", width: 102, render: (_, run) => formatDuration(run) },
    {
      title: "操作", width: 104,
      render: (_, run) => !isTerminal(run) && (
        <Popconfirm
          title={`取消 ${run.code} 的本次执行？`} okText="确认取消" cancelText="返回"
          onConfirm={() => cancel(run.run_id)} okButtonProps={{ danger: true }}
        >
          <Button size="small" disabled={cancelling !== null || pendingCancel !== null} loading={cancelling === run.run_id}>取消</Button>
        </Popconfirm>
      ),
    },
  ];

  return (
    <>
      <a className="back-link" href="#/tasks">返回任务列表</a>
      <div className="page-heading"><Typography.Title level={3}>任务详情</Typography.Title></div>
      {error ? <LoadError title="无法加载任务详情" error={error} /> : !task ? <Skeleton active /> : (
        <div className="page-stack">
          <Descriptions
            className="surface context-panel" size="small" column={{ xs: 1, sm: 2, lg: 3 }}
            items={[
              { key: "project", label: "项目", children: task.project },
              { key: "node", label: "节点", children: <code title={task.node_id}>{task.node_id.slice(0, 8)}</code> },
              { key: "created", label: "创建时间", children: formatDate(task.created_at) },
              { key: "executor", label: "执行员模型", children: task.executor.model ?? "模型来源默认" },
              { key: "effort", label: "推理强度", children: task.executor.effort ?? "模型来源默认" },
              { key: "judge", label: "判定员模型", children: [task.judge.model ?? "模型来源默认", task.judge.effort].filter(Boolean).join(" · ") },
            ]}
          />
          {actionError && <Alert type="error" showIcon title={actionError} />}
          <Table<RunView> rowKey="run_id" columns={columns} dataSource={task.runs} pagination={false} scroll={{ x: 1034 }} />
        </div>
      )}
      {pendingCancel && (
        <ReviewTokenDialog onConfirm={(token) => cancel(pendingCancel, token)} onCancel={() => setPendingCancel(null)} />
      )}
    </>
  );
}
