import { Button, Input, Modal, Select } from "antd";
import { useEffect, useState } from "react";
import { createUser, fetchNodes, loginUser, logoutUser, type NodeView } from "./api";
import { cleanName, formatExpiry, NEED_LOGIN_MESSAGE, readUserName, readUserToken, rememberUserName } from "./identity";

const linkStyle = { background: "none", border: "none", padding: 0, color: "var(--accent)", cursor: "pointer", font: "inherit", textDecoration: "underline" } as const;

interface Props {
  userName: string;
  expiresAt: number | null;
  nameProblem?: string | null;
  onForget: () => void;
  onCreated?: (name: string) => void;
  onLogin?: () => void;
  className?: string;
}

type Dialog = { kind: "create" } | { kind: "login"; name: string };

/** 「我是谁」旁边的名字记忆、创建和登录入口。 */
export default function IdentityHint({ userName, expiresAt, nameProblem, onForget, onCreated, onLogin, className }: Props) {
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [nodes, setNodes] = useState<NodeView[]>([]);
  const [claim, setClaim] = useState<string | undefined>();
  const [error, setError] = useState("");
  const [logoutError, setLogoutError] = useState("");
  const creating = dialog?.kind === "create";
  const submitLabel = creating ? "创建" : "登录";
  const currentName = cleanName(userName);

  // 只有创建时需要列出可认领的节点。
  useEffect(() => {
    if (!creating) return;
    const ctl = new AbortController();
    fetchNodes(ctl.signal).then((all) => {
      if (!ctl.signal.aborted) setNodes(all.filter((n) => n.online && !n.owner));
    }).catch(() => {
      if (!ctl.signal.aborted) setNodes([]);
    });
    return () => ctl.abort();
  }, [creating]);

  const close = () => {
    setDialog(null);
    setName("");
    setPassword("");
    setClaim(undefined);
    setNodes([]);
    setError("");
  };

  const clear = () => {
    setLogoutError("");
    void logoutUser(currentName || readUserName()).catch(() => setLogoutError("服务端注销失败，浏览器已清除会话"));
    onForget();
  };

  const submit = async () => {
    if (!dialog || busy) return;
    const clean = dialog.kind === "create" ? cleanName(name) : dialog.name;
    if (!clean) return setError("名字要 1 到 40 个字，不能有换行");
    if (creating && [...password].length < 6) return setError("密码至少 6 个字符");
    if (!password) return setError("请输入密码");
    setBusy(true);
    setPassword("");
    setError("");
    let created = "";
    try {
      if (creating) created = await createUser(clean, password, claim);
      const signedIn = await loginUser(created || clean, password);
      rememberUserName(signedIn);
      if (created) onCreated?.(signedIn);
      onLogin?.();
      close();
    } catch (e) {
      const message = e instanceof Error ? e.message : "请求失败";
      if (created) {
        // 名字已经建好，重试时直接登录。
        rememberUserName(created);
        onCreated?.(created);
        setDialog({ kind: "login", name: created });
        setError(`名字已创建，自动登录失败。${message}`);
      } else {
        setError(message);
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <span className={className} role="status">
      {expiresAt ? `已记住，到期 ${formatExpiry(expiresAt)}（只保存在这个浏览器）` : "填你的名字，和启动节点时设的 NODE_USER_NAME 一致；会在这个浏览器记 30 天"}{" "}
      {(expiresAt || readUserToken(currentName)) && <button type="button" onClick={clear} style={linkStyle}>清除</button>}{" "}
      {currentName && nameProblem === NEED_LOGIN_MESSAGE && (
        <button type="button" onClick={() => setDialog({ kind: "login", name: currentName })} style={linkStyle}>登录</button>
      )}{" "}
      {onCreated && (
        <button type="button" onClick={() => setDialog({ kind: "create" })} style={linkStyle}>创建名字</button>
      )}
      {logoutError && <span role="alert">{logoutError}</span>}
      <Modal title={creating ? "创建名字" : "登录"} open={dialog !== null} onCancel={close} footer={null} destroyOnHidden
        mask={{ closable: !busy }} keyboard={!busy} closable={!busy}>
        <div style={{ display: "grid", gap: 12 }}>
          {creating ? (
            <Input placeholder="名字（1 到 40 个字）" value={name} onChange={(e) => setName(e.target.value)} maxLength={40} disabled={busy} />
          ) : dialog?.kind === "login" && <div>名字 {dialog.name}</div>}
          <Input.Password aria-label="密码" placeholder={creating ? "密码（至少 6 个字符）" : "密码"} value={password}
            onChange={(e) => setPassword(e.target.value)} onPressEnter={submit} autoComplete="off" disabled={busy} />
          {creating && (
            <Select
              allowClear
              placeholder={nodes.length ? "认领一个没有主人的在线节点（可不选）" : "现在没有可认领的节点（节点没设 NODE_USER_NAME 才没有主人）"}
              value={claim}
              onChange={setClaim}
              options={nodes.map((n) => ({ value: n.node_id, label: `${n.caps.os} · ${n.node_id.slice(0, 8)}` }))}
              disabled={busy}
            />
          )}
          {error && <div role="alert" style={{ color: "var(--danger, #cf1322)" }}>{error}</div>}
          <Button type="primary" aria-label={submitLabel} aria-busy={busy} disabled={busy} loading={busy} onClick={submit}>{submitLabel}</Button>
        </div>
      </Modal>
    </span>
  );
}
