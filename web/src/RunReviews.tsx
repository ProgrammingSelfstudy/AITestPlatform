import { Alert, Button, Input, Modal, Radio, Space, Tag, Typography } from "antd";
import { createContext, useCallback, useContext, useState, type ReactNode } from "react";
import { fetchRunReviews, submitReview, type ReviewMark, type ReviewRecord, type RunReviews } from "./api";
import { browserStorage, readReviewToken, readReviewValue, rememberReviewToken, writeReviewValue } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";
import { formatDate, VerdictTag } from "./runUi";
import { usePolling } from "./usePolling";

const labels: Record<ReviewMark, string> = { correct: "判对", passed: "应为通过", failed: "应为失败" };
const markOptions = (Object.keys(labels) as ReviewMark[]).map((value) => ({ value, label: labels[value] }));
const READ_ONLY = "未授权，只能查看";
let sessionReviewer: string | undefined;

interface Target {
  itemIdx: number | null;
  label: string;
}

const ReviewContext = createContext<{
  data: RunReviews | null;
  error: string | null;
  authorized: boolean;
  finished: boolean;
  open: (target: Target) => void;
} | null>(null);

export function ReviewWorkspace({ runId, finished, children }: { runId: string; finished: boolean; children: ReactNode }) {
  const load = useCallback((signal: AbortSignal) => fetchRunReviews(runId, signal), [runId]);
  const { data, error, reload } = usePolling(load, 5000);
  const [token, setToken] = useState(readReviewToken);
  const [target, setTarget] = useState<Target | null>(null);

  function rememberToken(value: string) {
    rememberReviewToken(value);
    setToken(value);
  }

  return (
    <ReviewContext.Provider value={{ data, error, authorized: !!token, finished, open: setTarget }}>
      {children}
      {target && (
        <ReviewEditor runId={runId} target={target} token={token} finished={finished} rememberToken={rememberToken}
          current={data?.current.find((row) => row.item_idx === target.itemIdx)} onClose={() => setTarget(null)}
          onSaved={() => { setTarget(null); reload(); }}
        />
      )}
    </ReviewContext.Provider>
  );
}

function ReviewLine({ row }: { row: ReviewRecord }) {
  return (
    <div className="review-line wrap-text">
      <Space size={[4, 4]} wrap>
        <Tag color={row.mark === "correct" ? "green" : row.mark === "passed" ? "blue" : "red"}>{labels[row.mark]}</Tag>
        <Typography.Text>审核人：{row.reviewer}</Typography.Text>
        <Typography.Text type="secondary">{formatDate(row.created_at)}</Typography.Text>
        <Typography.Text type="secondary">提交时判定</Typography.Text>
        <VerdictTag verdict={row.judge_verdict} />
      </Space>
      {row.note && <div className="wrap-text">{row.note}</div>}
    </div>
  );
}

export function ReviewPanel({ itemIdx, label }: Target) {
  const scope = useContext(ReviewContext);
  if (!scope) return null;
  const { data, error, authorized, finished, open } = scope;
  const current = data?.current.find((row) => row.item_idx === itemIdx);
  const history = data?.history.filter((row) => row.item_idx === itemIdx) ?? [];
  return (
    <div className="review-block">
      <div className="panel-heading">
        <Typography.Text strong>当前标注</Typography.Text>
        <Button size="small" disabled={!finished} onClick={() => open({ itemIdx, label })} aria-label={`审核${label}`}>审核</Button>
      </div>
      {itemIdx === null && !authorized && <Typography.Text type="secondary">{READ_ONLY}</Typography.Text>}
      {itemIdx === null && !finished && <Typography.Text type="secondary">执行状态为「已结束」后才可提交审核。</Typography.Text>}
      {current ? <ReviewLine row={current} /> : (
        <Typography.Text type={error ? "danger" : "secondary"}>
          {error ? "无法加载审核记录" : data ? "尚未审核" : "正在加载审核记录…"}
        </Typography.Text>
      )}
      <details className="case-steps">
        <summary>审核历史（{history.length}）</summary>
        {history.length > 0
          ? <ul className="review-history">{history.map((row) => <li key={row.id}><ReviewLine row={row} /></li>)}</ul>
          : <Typography.Text type="secondary">没有审核记录</Typography.Text>}
      </details>
    </div>
  );
}

interface EditorProps {
  runId: string;
  target: Target;
  current?: ReviewRecord;
  token: string;
  finished: boolean;
  rememberToken: (token: string) => void;
  onClose: () => void;
  onSaved: () => void;
}

function ReviewEditor({ runId, target, current, token, finished, rememberToken, onClose, onSaved }: EditorProps) {
  const [mark, setMark] = useState<ReviewMark | undefined>(current?.mark);
  const [note, setNote] = useState(current?.note ?? "");
  const [reviewer, setReviewer] = useState(() => sessionReviewer ?? readReviewValue(browserStorage, "reviewer"));
  const [askingToken, setAskingToken] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function save() {
    if (saving || !finished) return;
    setError(null);
    if (!token) {
      setAskingToken(true);
      return;
    }
    if (!mark) {
      setError("请选择人工标注");
      return;
    }
    if (!reviewer.trim() || Array.from(reviewer).length > 32 || /\p{Cc}/u.test(reviewer)) {
      setError("审核人姓名须为 1～32 个字符，且不能含控制字符");
      return;
    }
    if (Array.from(note).length > 2000 || note.includes("\0")) {
      setError("备注最多 2000 个字符，且不能含空字符");
      return;
    }
    setSaving(true);
    try {
      const status = await submitReview(runId, { item_idx: target.itemIdx, mark, note: note || null, reviewer: reviewer.trim() }, token);
      if (status === 201) {
        sessionReviewer = reviewer.trim();
        writeReviewValue(browserStorage, "reviewer", sessionReviewer);
        onSaved();
      } else if (status === 403) {
        rememberToken("");
        setError("口令不对或审核提交已关闭");
      } else if (status === 429) {
        setError("口令尝试次数过多，请稍后再试");
      } else if (status === 409) {
        setError("执行尚未结束，无法提交审核");
      } else if (status === 400) {
        setError("审核内容或判定条目不符合要求，请检查后重试");
      } else {
        setError("审核提交失败，请稍后再试");
      }
    } catch {
      setError("无法提交审核，请检查连接后重试");
    } finally {
      setSaving(false);
    }
  }

  if (askingToken) {
    return (
      <ReviewTokenDialog disabled={!finished} onCancel={onClose}
        onConfirm={(value) => { rememberToken(value); setAskingToken(false); }}
      />
    );
  }

  const disabled = !token || !finished || saving;
  return (
    <Modal open title={`审核 · ${target.label}`}
      okText="保存" onOk={save} onCancel={() => { if (!saving) onClose(); }}
      okButtonProps={{ disabled: !finished }} confirmLoading={saving} cancelText="取消" width={520}>
      <div className="review-form">
        {error && <Alert type="error" title={error} showIcon />}
        {!token && <Alert type="info" title={READ_ONLY} description="点「保存」输入口令后可编辑。" showIcon />}
        <Radio.Group aria-label="人工标注" className="review-marks" options={markOptions} disabled={disabled}
          value={mark} onChange={(event) => setMark(event.target.value)} />
        <label>审核人姓名
          <Input disabled={disabled} value={reviewer} onChange={(event) => setReviewer(event.target.value)} />
        </label>
        <label>备注（可选）
          <Input.TextArea disabled={disabled} value={note} rows={3} onChange={(event) => setNote(event.target.value)} />
        </label>
      </div>
    </Modal>
  );
}
