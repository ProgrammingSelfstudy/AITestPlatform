import { Button, Empty, Input, Modal, Skeleton, Space, Table, Typography } from "antd";
import type { ColumnsType } from "antd/es/table";
import { useCallback, useState } from "react";
import { decideScript, fetchScripts, type ScriptRow, type ScriptStatus } from "./api";
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

const MODEL = { claude_cli: "Claude 命令行", codex_exec: "Codex" } as const;

interface Pending {
  script: ScriptRow;
  action: Action;
}

export default function ScriptsPage() {
  const load = useCallback((signal: AbortSignal) => fetchScripts(PROJECT, signal), []);
  const { data, error, reload } = usePolling(load, REFRESH_MS);
  const [pending, setPending] = useState<Pending | null>(null);
  const [reason, setReason] = useState("");
  const [askToken, setAskToken] = useState(false);
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

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
      title: "用例", dataIndex: "case_code", width: 220,
      render: (code: string, row) => (
        <div>
          <span className="code-tag">{code}</span>
          <div className="device-model-sub">{row.case_title}</div>
        </div>
      ),
    },
    { title: "版本", dataIndex: "version", width: 72, render: (v: number) => <span className="mono">v{v}</span> },
    {
      title: "状态", dataIndex: "status", width: 170,
      render: (status: ScriptStatus) => <Pill kind={STATUS[status].kind}>{STATUS[status].text}</Pill>,
    },
    {
      title: "生成", width: 200,
      render: (_, row) => (
        <span className="device-model-cell">
          <span className="device-model-name">{row.gen_model ?? MODEL[row.gen_source]}</span>
          <span className="device-model-sub mono">{row.gen_effort ?? "—"}</span>
        </span>
      ),
    },
    {
      title: "来源执行", dataIndex: "source_run_id", width: 110,
      render: (id: string) => <a className="run-code-link mono" href={`#/runs/${id}`}>{id.slice(0, 8)}</a>,
    },
    { title: "更新时间", dataIndex: "updated_at", width: 170, render: (t: string) => <span className="mono">{formatDate(t)}</span> },
    {
      title: "操作", key: "actions", width: 150, fixed: "right",
      render: (_, row) => (
        <Space size={8}>
          {ACTIONS[row.status].map((action) => (
            <Button key={action} size="small" type={action === "approve" ? "primary" : "default"} danger={action !== "approve"} onClick={() => open(row, action)}>
              {ACTION[action].label}
            </Button>
          ))}
          {ACTIONS[row.status].length === 0 && "—"}
        </Space>
      ),
    },
  ];

  return (
    <div className="nodes-page">
      <div className="page-heading">
        <div className="page-title-block">
          <Typography.Title level={3}>回归脚本</Typography.Title>
          <p className="page-desc">
            通过的执行会生成脚本版本：候选 → 在真机上自检 → 人工获准。只有「已获准」的版本才能被平台选来回归，
            同一用例同时只有一个获准版本。生成和自检在执行节点上进行，这里只做获准、下线和拒绝的决定。
          </p>
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
        <div className="surface">
          <Table<ScriptRow>
            rowKey="id"
            columns={columns}
            dataSource={data}
            pagination={false}
            scroll={{ x: 960 }}
            expandable={{
              expandedRowRender: (row) => (
                <div className="script-detail">
                  <p><strong>文件：</strong><span className="mono">{row.file_names.join("、")}</span>（依据用例版本 {row.case_version}）</p>
                  {row.notes && <p><strong>生成备注：</strong>{row.notes}</p>}
                  {row.status_note && (
                    <div>
                      <strong>状态说明（自检输出和人的决定）：</strong>
                      <pre className="script-note">{row.status_note}</pre>
                    </div>
                  )}
                </div>
              ),
            }}
          />
        </div>
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
      {pending && askToken && (
        <ReviewTokenDialog disabled={busy} onConfirm={(token) => submit(token)} onCancel={close} />
      )}
    </div>
  );
}
