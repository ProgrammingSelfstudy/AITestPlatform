import { Alert, Button, Input, Modal, Popconfirm, Select, Space, Table, Tag, Upload } from "antd";
import type { TableColumnsType } from "antd";
import { useEffect, useState } from "react";
import {
  createModuleCase,
  deleteModuleCase,
  fetchAccountIdentities,
  importModuleCases,
  moduleCasesCsvUrl,
  updateModuleCase,
  type AccountIdentityOption,
  type LatestRun,
  type ModuleCase,
  type ReviewStatus,
} from "./api";
import { useGuardedWrite } from "./useGuardedWrite";
import { useUnsavedWarning } from "./unsavedWarning";

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
  // 贴了内容还没导入：刷新或关页面前先问一声
  useUnsavedWarning(open && text.trim() !== "" && !result);

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

function splitLines(text: string): string[] {
  return text.replace(/\r\n/g, "\n").replace(/\r/g, "\n").split("\n").map((s) => s.trim()).filter(Boolean);
}

/** 单条用例新增 / 编辑弹窗：支持在线调整标题、运行环境、级别、前置条件、步骤和预期；离开前有未保存提醒 */
function CaseEditorDialog({
  moduleId,
  editing,
  open,
  userName,
  onClose,
  onDone,
}: {
  moduleId: string;
  editing: ModuleCase | null;
  open: boolean;
  userName: string;
  onClose: () => void;
  onDone: (notice: string) => void;
}) {
  const isEdit = editing !== null;
  const [code, setCode] = useState("");
  const [title, setTitle] = useState("");
  const [runEnv, setRunEnv] = useState<ModuleCase["run_env"]>("app");
  const [level, setLevel] = useState("");
  const [identity, setIdentity] = useState<string | undefined>(undefined);
  const [identities, setIdentities] = useState<AccountIdentityOption[]>([]);
  const [preconditions, setPreconditions] = useState("");
  const [steps, setSteps] = useState("");
  const [expected, setExpected] = useState("");
  const [dirty, setDirty] = useState(false);
  const write = useGuardedWrite(userName);

  useEffect(() => {
    if (!open) return;
    setCode(editing?.code ?? "");
    setTitle(editing?.title ?? "");
    setRunEnv(editing?.run_env ?? "app");
    setLevel(editing?.level ?? "");
    setIdentity(editing?.account_identity ?? undefined);
    setPreconditions((editing?.preconditions ?? []).join("\n"));
    setSteps((editing?.steps ?? []).join("\n"));
    setExpected((editing?.expected ?? []).join("\n"));
    setDirty(false);
    write.setError(null);
  }, [open, editing]);

  // 身份下拉的选项只来自参数配置「账号身份」；读不到就不给选，不影响其他字段
  useEffect(() => {
    if (!open) return;
    const controller = new AbortController();
    fetchAccountIdentities(userName.trim(), controller.signal)
      .then((r) => setIdentities(r.options))
      .catch(() => setIdentities([]));
    return () => controller.abort();
  }, [open, userName]);

  useUnsavedWarning(open && dirty);

  async function submit() {
    const payload = {
      title: title.trim(),
      run_env: runEnv,
      level: level.trim(),
      account_identity: identity ?? null,
      preconditions: splitLines(preconditions),
      steps: splitLines(steps),
      expected: splitLines(expected),
    };
    await write.run(async (token) => {
      if (isEdit && editing) {
        const res = await updateModuleCase(moduleId, editing.code, payload, token, userName);
        setDirty(false);
        onDone(res.updated ? `已更新用例 ${editing.code}（版本 v${res.version}）` : `用例 ${editing.code} 内容没有变化`);
        onClose();
      } else {
        const res = await createModuleCase(moduleId, { code: code.trim(), ...payload }, token, userName);
        setDirty(false);
        onDone(`已新增用例 ${res.code}（已批准，可直接发起测试）`);
        onClose();
      }
    });
  }

  return (
    <Modal
      title={isEdit ? `编辑用例：${editing?.code}` : "新增用例"}
      open={open}
      onCancel={onClose}
      destroyOnHidden
      width={680}
      footer={[
        <Button key="cancel" disabled={write.busy} onClick={onClose}>取消</Button>,
        <Button key="save" type="primary" loading={write.busy} onClick={() => void submit()}>{isEdit ? "保存修改" : "保存用例"}</Button>,
      ]}
    >
      <div className="mod-import">
        <label>
          <div className="mod-field-meta">编号（字母、数字、- _ .，同项目唯一）</div>
          <Input
            aria-label="用例编号"
            value={code}
            disabled={isEdit || write.busy}
            placeholder="例如：PROF-01"
            maxLength={40}
            onChange={(e) => { setCode(e.target.value); setDirty(true); }}
          />
        </label>
        <label>
          <div className="mod-field-meta">标题（必填）</div>
          <Input
            aria-label="用例标题"
            value={title}
            disabled={write.busy}
            placeholder="例如：修改昵称后重启应用保持生效"
            maxLength={200}
            onChange={(e) => { setTitle(e.target.value); setDirty(true); }}
          />
        </label>
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 12 }}>
          <label>
            <div className="mod-field-meta">运行环境</div>
            <Select<ModuleCase["run_env"]>
              aria-label="运行环境"
              style={{ width: "100%" }}
              value={runEnv}
              disabled={write.busy}
              onChange={(v) => { setRunEnv(v); setDirty(true); }}
              options={[
                { value: "app", label: "App" },
                { value: "h5", label: "H5" },
                { value: "browser", label: "浏览器" },
              ]}
            />
          </label>
          <label>
            <div className="mod-field-meta">级别（可选）</div>
            <Input
              aria-label="用例级别"
              value={level}
              disabled={write.busy}
              placeholder="例如：高 / 中 / 简单流程"
              maxLength={40}
              onChange={(e) => { setLevel(e.target.value); setDirty(true); }}
            />
          </label>
        </div>
        <label>
          <div className="mod-field-meta">账号身份（可选；选了就只用该身份的测试账号，选项在参数配置「账号身份」里维护）</div>
          <Select<string>
            aria-label="账号身份"
            style={{ width: "100%" }}
            value={identity}
            allowClear
            disabled={write.busy}
            placeholder={identities.length === 0 ? "还没有配置账号身份" : "不限身份"}
            onChange={(v) => { setIdentity(v); setDirty(true); }}
            options={[
              ...identities.map((i) => ({ value: i.code, label: `${i.name}（${i.code}）` })),
              // 用例上已有的身份被停用/删除后仍要能看到，不能悄悄变成「不限」
              ...(identity && !identities.some((i) => i.code === identity) ? [{ value: identity, label: `${identity}（已不在配置里）` }] : []),
            ]}
          />
        </label>
        <label>
          <div className="mod-field-meta">前置条件（一行一条，可选；账号密码只能用 {"{{account.xxx}}"} 占位符）</div>
          <Input.TextArea
            aria-label="前置条件"
            rows={2}
            value={preconditions}
            disabled={write.busy}
            placeholder="已登录测试账号 {{account.username}}"
            onChange={(e) => { setPreconditions(e.target.value); setDirty(true); }}
          />
        </label>
        <label>
          <div className="mod-field-meta">操作步骤（必填，一行一步）</div>
          <Input.TextArea
            aria-label="操作步骤"
            rows={4}
            value={steps}
            disabled={write.busy}
            placeholder={"点击个人资料\n点击昵称输入框并输入新昵称\n点击保存"}
            onChange={(e) => { setSteps(e.target.value); setDirty(true); }}
          />
        </label>
        <label>
          <div className="mod-field-meta">预期结果（必填，一行一条，判定员逐条核对）</div>
          <Input.TextArea
            aria-label="预期结果"
            rows={4}
            value={expected}
            disabled={write.busy}
            placeholder={"页面提示保存成功\n返回个人页显示新昵称"}
            onChange={(e) => { setExpected(e.target.value); setDirty(true); }}
          />
        </label>
        {write.error && <Alert type="error" showIcon title={write.error} />}
      </div>
      {write.dialog}
    </Modal>
  );
}

/** 测试用例：逐条呈现每一条（不截断、不折叠），支持在线新增 / 编辑 / 删除，以及导出、导入 CSV。 */
export default function ModuleCases({ moduleId, cases, userName, onChanged }: { moduleId: string; cases: ModuleCase[]; userName: string; onChanged: () => void }) {
  const [importing, setImporting] = useState(false);
  const [editorOpen, setEditorOpen] = useState(false);
  const [editing, setEditing] = useState<ModuleCase | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [pageSize, setPageSize] = useState(50);
  const trimmedName = userName.trim();
  const deleteWrite = useGuardedWrite(trimmedName);

  async function removeCase(code: string) {
    setNotice(null);
    await deleteWrite.run(async (token) => {
      const res = await deleteModuleCase(moduleId, code, token, trimmedName);
      setNotice(res.archived ? `用例 ${code} 已有历史执行或脚本记录，已归档隐藏（历史数据保留）` : `已删除用例 ${code}`);
      onChanged();
    });
  }

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
    {
      title: "操作",
      key: "actions",
      width: 136,
      fixed: "right",
      render: (_: unknown, row) => (
        <Space size={4}>
          <Button
            size="small"
            onClick={() => {
              setNotice(null);
              setEditing(row);
              setEditorOpen(true);
            }}
          >
            编辑
          </Button>
          <Popconfirm
            title={`删除用例 ${row.code}？`}
            description="若有历史执行或脚本，将自动归档隐藏以保留证据。"
            okText="删除"
            cancelText="取消"
            okButtonProps={{ danger: true }}
            onConfirm={() => void removeCase(row.code)}
          >
            <Button size="small" danger disabled={deleteWrite.busy}>删除</Button>
          </Popconfirm>
        </Space>
      ),
    },
  ];

  return (
    <section className="mod-panel" aria-label="测试用例">
      <div className="mod-panel-head">
        <h2 className="mod-panel-title">测试用例（共 {cases.length} 条）</h2>
        <div className="mod-actions">
          <Button
            type="primary"
            onClick={() => {
              setNotice(null);
              setEditing(null);
              setEditorOpen(true);
            }}
          >
            新增用例
          </Button>
          <a className="ant-btn ant-btn-default" href={moduleCasesCsvUrl(moduleId)} download="cases.csv">导出 CSV</a>
          <Button onClick={() => setImporting(true)}>导入 CSV</Button>
        </div>
      </div>
      {notice && <Alert type="success" showIcon closable onClose={() => setNotice(null)} title={notice} style={{ margin: "0 20px 12px" }} />}
      {deleteWrite.error && <Alert type="error" showIcon closable onClose={() => deleteWrite.setError(null)} title={deleteWrite.error} style={{ margin: "0 20px 12px" }} />}
      <Table<ModuleCase>
        rowKey="code"
        size="small"
        columns={columns}
        dataSource={cases}
        scroll={{ x: 1560 }}
        locale={{ emptyText: "这个模块还没有用例：点「新增用例」、新建任务生成用例，或点「导入 CSV」" }}
        pagination={{ pageSize, showSizeChanger: true, pageSizeOptions: [20, 50, 100, 200], showTotal: (n) => `共 ${n} 条`, onShowSizeChange: (_p, size) => setPageSize(size) }}
      />
      <CaseEditorDialog
        moduleId={moduleId}
        editing={editing}
        open={editorOpen}
        userName={trimmedName}
        onClose={() => setEditorOpen(false)}
        onDone={(msg) => {
          setNotice(msg);
          onChanged();
        }}
      />
      <ImportDialog moduleId={moduleId} userName={trimmedName} open={importing} onClose={() => setImporting(false)} onDone={onChanged} />
      {deleteWrite.dialog}
    </section>
  );
}
