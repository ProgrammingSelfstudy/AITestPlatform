import { Alert, Button, Input } from "antd";
import { useCallback, useEffect, useRef, useState } from "react";
import { confirmProposal, dismissProposal, fetchAdminChat, sendAdminMessage, type AdminChat, type AdminMessage, type AdminProposal } from "./api";
import GenerateDialog from "./GenerateDialog";
import IdentityHint from "./IdentityHint";
import { useRememberedUserName } from "./identity";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";
import { formatTime } from "./runUi";
import { usePolling } from "./usePolling";
import "./admin.css";

/**
 * 管理员：读需求文档和各模块现状，和你聊天，**只提议，你点确认才执行**。
 * - 管理员没有任何工具：不能读文件、不能调接口、不能自己派任务，也不能批准用例；
 * - 提议是卡片：「确认」才由服务端用你的身份、在你的节点上执行；「忽略」不执行任何东西；
 * - 回答由你自己节点上的管理员角色生成，花的是你自己的额度（规则 9）。
 */

const STATUS_TEXT: Record<AdminProposal["status"], string> = {
  pending: "待你确认",
  confirmed: "已确认",
  dismissed: "已忽略",
  failed: "执行失败",
};

function describe(p: AdminProposal): { title: string; detail: string } {
  if (p.kind === "create_module") return { title: `新建模块「${p.args.name ?? ""}」`, detail: "确认后建一个空模块，之后可以给它生成用例" };
  if (p.kind === "generate_cases") return { title: `给模块「${p.args.module ?? ""}」生成用例`, detail: "需要你补上 UI 链接和区域截图，所以确认后会打开预填好的表单，由你提交" };
  const codes = p.args.case_codes ?? [];
  return { title: `对模块「${p.args.module ?? ""}」发起测试（${codes.length} 条已批准的用例）`, detail: `确认后用你的节点建测试任务：${codes.join("、")}` };
}

type Pending = { kind: "send"; text: string } | { kind: "confirm"; proposal: AdminProposal };

export default function AdminPage() {
  const { name: userName, setName: setUserName, expiresAt, forget } = useRememberedUserName();
  const trimmedName = userName.trim();
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [pendingWrite, setPendingWrite] = useState<Pending | null>(null);
  const [generateFor, setGenerateFor] = useState<string | null>(null);
  const lock = useRef(false);
  const bottom = useRef<HTMLDivElement | null>(null);

  // 对话轮询；内容没变就复用旧引用，页面不闪
  const last = useRef<{ key: string; value: AdminChat } | null>(null);
  const load = useCallback(
    async (signal: AbortSignal): Promise<AdminChat | null> => {
      if (!trimmedName) return null;
      const next = await fetchAdminChat(trimmedName, signal);
      const key = JSON.stringify(next);
      if (last.current?.key === key) return last.current.value;
      last.current = { key, value: next };
      return next;
    },
    [trimmedName],
  );
  const { data: chat, error: loadError, reload } = usePolling(load, 1500);
  const messages = chat?.messages ?? [];
  const waiting = Boolean(chat?.pending);

  // 有新消息就滚到底
  useEffect(() => {
    bottom.current?.scrollIntoView?.({ block: "end" });
  }, [messages.length, waiting]);

  async function run(action: Pending, entered?: string) {
    if (lock.current || (pendingWrite && entered === undefined)) return;
    const token = entered ?? readReviewToken();
    const step = writeTokenAction(Boolean(trimmedName) && !busy, token);
    if (step === "blocked") {
      setPendingWrite(null);
      return;
    }
    setError(null);
    if (step === "prompt") {
      setPendingWrite(action);
      return;
    }
    if (entered !== undefined) rememberReviewToken(entered);
    lock.current = true;
    setPendingWrite(null);
    setBusy(true);
    setNotice(null);
    try {
      if (action.kind === "send") {
        await sendAdminMessage(action.text, token, trimmedName);
        setDraft("");
      } else {
        const done = await confirmProposal(action.proposal.id, token, trimmedName);
        setNotice(done.result);
        // 生成用例要 UI 链接和截图，管理员拿不到：打开预填好模块名的表单，由你补上再提交
        if (done.next?.open_generate) setGenerateFor(done.next.open_generate.module);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      lock.current = false;
      setBusy(false);
      reload();
    }
  }

  async function dismiss(p: AdminProposal) {
    setError(null);
    try {
      await dismissProposal(p.id, trimmedName);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      reload();
    }
  }

  function submit() {
    const text = draft.trim();
    if (!text || waiting) return;
    void run({ kind: "send", text });
  }

  return (
    <div className="adm-page">
      <header className="adm-head">
        <div>
          <h1 className="adm-title">管理员</h1>
          <p className="adm-sub">新增或修改需求，直接说给它。它读需求文档和各模块的现状，给你提议；<strong>你点「确认」才会真的动手</strong>，它自己什么也不会做。</p>
        </div>
        <span className="adm-badge">只提议，不动手</span>
      </header>

      <div className="adm-who">
        <label htmlFor="adm-name">我是谁</label>
        <Input id="adm-name" aria-label="我是谁" autoComplete="off" allowClear maxLength={40} value={userName} onChange={(e) => setUserName(e.target.value)} placeholder="你的名字（和启动节点时设的 NODE_USER_NAME 一致）" />
        <IdentityHint userName={userName} expiresAt={expiresAt} nameProblem={loadError} onForget={forget}
          onCreated={setUserName} onLogin={() => { setError(null); reload(); }} className="adm-who-hint" />
      </div>

      {!trimmedName && <Alert type="info" showIcon title="先填「我是谁」" description="对话是各人自己的；回答由你自己节点上的管理员角色生成，用你自己的额度。你的节点要先在「角色与模型」页配一个「管理员」角色并检测通过。" />}
      {loadError && <Alert type="error" showIcon title="无法读取对话" description={loadError} />}
      {error && <Alert type="error" showIcon title="没有成功" description={<span className="adm-wrap">{error}</span>} closable onClose={() => setError(null)} />}
      {notice && <Alert type="success" showIcon title="已执行" description={<span className="adm-wrap">{notice}</span>} closable onClose={() => setNotice(null)} />}

      <section className="adm-chat" aria-label="和管理员的对话" aria-live="polite">
        {trimmedName && messages.length === 0 && !waiting && (
          <div className="adm-empty">
            还没有对话。比如说：「需求文档更新了，新增了充值功能，帮我安排一下」「语聊房现在能测吗？」
          </div>
        )}
        {messages.map((m) => (
          <Bubble key={m.id} message={m} busy={busy} onConfirm={(p) => void run({ kind: "confirm", proposal: p })} onDismiss={(p) => void dismiss(p)} />
        ))}
        {waiting && (
          <div className="adm-row is-admin" role="status">
            <div className="adm-bubble is-admin">
              <span className="adm-dot" aria-hidden="true" /> 管理员正在读现状、想怎么回答…（用的是你节点上的模型）
            </div>
          </div>
        )}
        <div ref={bottom} />
      </section>

      <footer className="adm-composer">
        <Input.TextArea
          aria-label="对管理员说"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onPressEnter={(e) => {
            if (!e.shiftKey) {
              e.preventDefault();
              submit();
            }
          }}
          placeholder={waiting ? "管理员还在回答上一句，等它回完再说" : "说说新的或改动的需求（回车发送，Shift+回车换行）"}
          autoSize={{ minRows: 2, maxRows: 8 }}
          maxLength={4000}
          disabled={!trimmedName || waiting}
        />
        <Button type="primary" onClick={submit} disabled={!trimmedName || waiting || busy || !draft.trim()} loading={busy}>发送</Button>
      </footer>

      {pendingWrite && <ReviewTokenDialog disabled={busy} onConfirm={(token) => void run(pendingWrite, token)} onCancel={() => setPendingWrite(null)} />}
      {generateFor !== null && <GenerateDialog initialModule={generateFor} onClose={() => setGenerateFor(null)} />}
    </div>
  );
}

function Bubble({ message, busy, onConfirm, onDismiss }: { message: AdminMessage; busy: boolean; onConfirm: (p: AdminProposal) => void; onDismiss: (p: AdminProposal) => void }) {
  const mine = message.author === "user";
  return (
    <div className={`adm-row ${mine ? "is-user" : "is-admin"}`}>
      <div className={`adm-bubble ${mine ? "is-user" : "is-admin"}${message.is_error ? " is-error" : ""}`}>
        <div className="adm-meta">
          {mine ? "你" : message.is_error ? "管理员（出错了）" : "管理员"} · {formatTime(message.created_at)}
        </div>
        <div className="adm-text">{message.text}</div>
        {message.proposals.length > 0 && (
          <ul className="adm-proposals" aria-label="管理员的提议">
            {message.proposals.map((p) => {
              const d = describe(p);
              return (
                <li key={p.id} className={`adm-proposal is-${p.status}`}>
                  <div className="adm-proposal-head">
                    <strong>{d.title}</strong>
                    <span className={`adm-status is-${p.status}`}>{STATUS_TEXT[p.status]}</span>
                  </div>
                  {p.why && <div className="adm-why">理由：{p.why}</div>}
                  <div className="adm-detail">{d.detail}</div>
                  {p.result && <div className={`adm-result${p.status === "failed" ? " is-failed" : ""}`}>{p.status === "failed" ? "失败原因：" : "结果："}{p.result}</div>}
                  {p.status === "pending" && (
                    <div className="adm-actions">
                      <Button type="primary" size="small" disabled={busy} onClick={() => onConfirm(p)}>确认</Button>
                      <Button size="small" disabled={busy} onClick={() => onDismiss(p)}>忽略</Button>
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </div>
  );
}
