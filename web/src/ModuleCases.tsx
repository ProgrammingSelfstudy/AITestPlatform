import { Alert, Button, Input, Modal, Table, Tag, Upload } from "antd";
import type { TableColumnsType } from "antd";
import { useState } from "react";
import { importModuleCases, moduleCasesCsvUrl, type LatestRun, type ModuleCase, type ReviewStatus } from "./api";
import { useGuardedWrite } from "./useGuardedWrite";

export const RUN_ENV_META: Record<ModuleCase["run_env"], { text: string; color: string }> = {
  app: { text: "App", color: "blue" },
  h5: { text: "H5", color: "purple" },
  browser: { text: "浏览器", color: "geekblue" },
};

export const REVIEW_TEXT: Record<ReviewStatus, string> = {
  draft: "草稿",
  reviewed_ok: "评审员认为没问题（待你确认）",
  needs_human: "需要人工确认",
  approved: "已批准",
  rejected: "已拒绝",
};

export const RESULT_TEXT: Record<NonNullable<LatestRun["final_result"]>, string> = {
  passed: "通过",
  failed: "失败",
  uncertain: "待人工确认",
  blocked: "受阻",
};

export function resultText(run: LatestRun | null): string {
  if (!run) return "未执行";
  if (run.blocked_reason === "skipped") return "已跳过";
  if (run.final_result) return RESULT_TEXT[run.final_result];
  return "进行中";
}

/** 步骤、预期这类多行内容：一行一条，带编号，完整显示不截断 */
function Lines({ items }: { items: string[] }) {
  if (items.length === 0) return <span className="mod-field-meta">—</span>;
  return (
    <ol className="mod-case-lines">
      {items.map((line, i) => <li key={i}>{line}</li>)}
    </ol>
  );
}

/** 导入 CSV：选文件或粘贴；整体校验，有问题整个文件都不写，错误里写明第几行 */
function ImportDialog({ moduleId, userName, open, onClose, onDone }: { moduleId: string; userName: string; open: boolean; onClose: () => void; onDone: () => void }) {
  const [text, setText] = useState("");
  const [result, setResult] = useState<string | null>(null);
  const write = useGuardedWrite(userName);

  function close() {
    setText("");
    setResult(null);
    write.setError(null);
    onClose();
  }

  async function submit() {
    setResult(null);
    await write.run(async (token) => {
      const done = await importModuleCases(moduleId, text, token, userName);
      setResult(`导入完成：新增 ${done.inserted} 条，更新 ${done.updated} 条，没变 ${done.unchanged} 条`);
      onDone();
    });
  }

  return (
    <Modal
      title="导入用例 CSV"
      open={open}
      onCancel={close}
      destroyOnHidden
      width={680}
      footer={result ? <Button type="primary" onClick={close}>完成</Button> : [
        <Button key="c" onClick={close}>取消</Button>,
        <Button key="ok" type="primary" loading={write.busy} disabled={!text.trim()} onClick={() => void submit()}>导入</Button>,
      ]}
    >
      {result ? (
        <Alert type="success" showIcon title={result} />
      ) : (
        <div className="mod-import">
          <p className="mod-field-meta">
            表头：编号、标题、运行环境（app / h5 / browser，留空按 app）、级别、前置条件、步骤、预期。步骤、预期里一行一条（单元格里换行）。
            「复核状态」「最近结果」两列只用于导出，导入时忽略。先点「导出 CSV」可以拿到模板。导入的用例是人给的，直接算「已批准」。
            文件里任何一行不合格，整个文件都不会导入。
          </p>
          <Upload accept=".csv,text/csv" showUploadList={false} beforeUpload={(file) => { void file.text().then(setText); return false; }}>
            <Button>选择 CSV 文件</Button>
          </Upload>
          <Input.TextArea rows={10} value={text} onChange={(e) => setText(e.target.value)} aria-label="CSV 内容" className="mono" placeholder="编号,标题,运行环境,级别,前置条件,步骤,预期" />
          {write.error && <Alert type="error" showIcon title={write.error} />}
        </div>
      )}
      {write.dialog}
    </Modal>
  );
}

/** 测试用例：CSV 表格形式，前端逐条呈现每一条（不截断、不折叠），可以导出、导入 CSV。 */
export default function ModuleCases({ moduleId, cases, userName, onChanged }: { moduleId: string; cases: ModuleCase[]; userName: string; onChanged: () => void }) {
  const [importing, setImporting] = useState(false);
  const [pageSize, setPageSize] = useState(50);

  const columns: TableColumnsType<ModuleCase> = [
    { title: "编号", dataIndex: "code", width: 150, fixed: "left", render: (v: string) => <span className="mono" style={{ whiteSpace: "nowrap" }}>{v}</span> },
    { title: "标题", dataIndex: "title", width: 200 },
    { title: "运行环境", dataIndex: "run_env", width: 90, render: (env: ModuleCase["run_env"]) => <Tag color={RUN_ENV_META[env].color}>{RUN_ENV_META[env].text}</Tag> },
    { title: "级别", dataIndex: "level", width: 80, render: (v: string) => v || "—" },
    { title: "前置条件", dataIndex: "preconditions", width: 180, render: (v: string[]) => <Lines items={v} /> },
    { title: "步骤", dataIndex: "steps", width: 280, render: (v: string[]) => <Lines items={v} /> },
    { title: "预期", dataIndex: "expected", width: 240, render: (v: string[]) => <Lines items={v} /> },
    { title: "复核状态", dataIndex: "review_status", width: 150, render: (v: ReviewStatus) => REVIEW_TEXT[v] },
    { title: "最近结果", key: "result", width: 100, render: (_: unknown, row) => resultText(row.latest_run) },
  ];

  return (
    <section className="mod-panel" aria-label="测试用例">
      <div className="mod-panel-head">
        <h2 className="mod-panel-title">测试用例（共 {cases.length} 条）</h2>
        <div className="mod-actions">
          <a className="ant-btn ant-btn-default" href={moduleCasesCsvUrl(moduleId)} download="cases.csv">导出 CSV</a>
          <Button type="primary" onClick={() => setImporting(true)}>导入 CSV</Button>
        </div>
      </div>
      <Table<ModuleCase>
        rowKey="code"
        size="small"
        columns={columns}
        dataSource={cases}
        scroll={{ x: 1400 }}
        locale={{ emptyText: "这个模块还没有用例：新建任务生成用例，或点「导入 CSV」" }}
        pagination={{ pageSize, showSizeChanger: true, pageSizeOptions: [20, 50, 100, 200], showTotal: (n) => `共 ${n} 条`, onShowSizeChange: (_p, size) => setPageSize(size) }}
      />
      <ImportDialog moduleId={moduleId} userName={userName.trim()} open={importing} onClose={() => setImporting(false)} onDone={onChanged} />
    </section>
  );
}
