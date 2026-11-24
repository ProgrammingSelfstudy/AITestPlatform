import { Table, Tag } from "antd";
import type { TableColumnsType } from "antd";
import { formatDate } from "./runUi";
import type { ModuleCase } from "./api";
import { RESULT_TEXT, RUN_ENV_META, resultText } from "./ModuleCases";

const RESULT_COLOR: Record<string, string> = { passed: "green", failed: "red", uncertain: "orange", blocked: "default" };
const skipped = (c: ModuleCase) => c.latest_run?.blocked_reason === "skipped";

/** 测试结果：每条用例一行，显示最近一次运行的结果；没跑过的写「未执行」，不会算成通过。 */
export default function ModuleResults({ cases }: { cases: ModuleCase[] }) {
  const count = (pick: (c: ModuleCase) => boolean) => cases.filter(pick).length;
  const summary = [
    { text: "通过", n: count((c) => c.latest_run?.final_result === "passed"), color: "green" },
    { text: "失败", n: count((c) => c.latest_run?.final_result === "failed"), color: "red" },
    { text: "待人工确认", n: count((c) => c.latest_run?.final_result === "uncertain"), color: "orange" },
    { text: "受阻", n: count((c) => c.latest_run?.final_result === "blocked" && !skipped(c)), color: "default" },
    { text: "已跳过", n: count(skipped), color: "gold" },
    { text: "进行中", n: count((c) => Boolean(c.latest_run && !c.latest_run.final_result)), color: "blue" },
    { text: "未执行", n: count((c) => !c.latest_run), color: "default" },
  ];
  const columns: TableColumnsType<ModuleCase> = [
    { title: "编号", dataIndex: "code", width: 90, render: (v: string) => <span className="mono">{v}</span> },
    { title: "标题", dataIndex: "title" },
    { title: "运行环境", dataIndex: "run_env", width: 90, render: (env: ModuleCase["run_env"]) => <Tag color={RUN_ENV_META[env].color}>{RUN_ENV_META[env].text}</Tag> },
    {
      title: "最近结果",
      key: "result",
      width: 120,
      render: (_: unknown, row) => <Tag color={skipped(row) ? "gold" : RESULT_COLOR[row.latest_run?.final_result ?? ""] ?? "blue"}>{resultText(row.latest_run)}</Tag>,
    },
    {
      title: "受阻 / 跳过原因",
      key: "blocked",
      width: 320,
      render: (_: unknown, row) => row.latest_run?.skip_reason ?? row.latest_run?.blocked_reason ?? "—",
    },
    { title: "时间", key: "time", width: 180, render: (_: unknown, row) => (row.latest_run ? formatDate(row.latest_run.queued_at) : "—") },
    { title: "详情", key: "link", width: 90, render: (_: unknown, row) => (row.latest_run ? <a className="mod-link" href={`#/runs/${row.latest_run.run_id}`}>查看</a> : "—") },
  ];
  return (
    <section className="mod-panel" aria-label="测试结果">
      <div className="mod-panel-head">
        <h2 className="mod-panel-title">测试结果</h2>
        <div>{summary.map((s) => <Tag key={s.text} color={s.color}>{s.text} {s.n}</Tag>)}</div>
      </div>
      <Table<ModuleCase> rowKey="code" size="small" columns={columns} dataSource={cases} pagination={{ pageSize: 50, showSizeChanger: true, pageSizeOptions: [20, 50, 100, 200], showTotal: (n) => `共 ${n} 条` }} locale={{ emptyText: "这个模块还没有用例" }} />
      <div className="mod-field-meta">结果文字：{Object.values(RESULT_TEXT).join(" / ")}；「未执行」「进行中」「已跳过」都不算通过。</div>
    </section>
  );
}
