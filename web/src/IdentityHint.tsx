import { Button, Input, Modal } from "antd";
import { useState } from "react";
import { createUser } from "./api";
import { cleanName, formatExpiry, rememberUserName } from "./identity";

const linkStyle = { background: "none", border: "none", padding: 0, color: "var(--accent)", cursor: "pointer", font: "inherit", textDecoration: "underline" } as const;

/**
 * 「我是谁」输入框旁边的说明：只记名字、靠自觉；记着就写到期日并给「清除」。
 * 名字不是机密——它只是标明谁在跑；各人用自己节点的额度。
 * 「创建名字」：名字还没登记时，输入名字和密码创建；创建成功后自动记住并回填（onCreated）。
 */
export default function IdentityHint({ expiresAt, onForget, onCreated, className }: { expiresAt: number | null; onForget: () => void; onCreated?: (name: string) => void; className?: string }) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const submit = async () => {
    const clean = cleanName(name);
    if (!clean) return setError("名字要 1 到 40 个字，不能有换行");
    if ([...password].length < 6) return setError("密码至少 6 个字符");
    setBusy(true);
    setError("");
    try {
      const created = await createUser(clean, password);
      rememberUserName(created);
      onCreated?.(created);
      setOpen(false);
      setName("");
      setPassword("");
    } catch (e) {
      setError(e instanceof Error ? e.message : "创建失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <span className={className} role="status">
      {expiresAt ? (
        <>
          已记住，到期 {formatExpiry(expiresAt)}（只保存在这个浏览器）{" "}
          <button type="button" onClick={onForget} style={linkStyle}>清除</button>
        </>
      ) : (
        "填你的名字，和启动节点时设的 NODE_USER_NAME 一致；会在这个浏览器记 30 天"
      )}{" "}
      {onCreated && (
        <>
          <button type="button" onClick={() => setOpen(true)} style={linkStyle}>创建名字</button>
          <Modal title="创建名字" open={open} onCancel={() => setOpen(false)} footer={null} destroyOnHidden>
            <div style={{ display: "grid", gap: 12 }}>
              <Input placeholder="名字（1 到 40 个字）" value={name} onChange={(e) => setName(e.target.value)} maxLength={40} />
              <Input.Password placeholder="密码（至少 6 个字符）" value={password} onChange={(e) => setPassword(e.target.value)} onPressEnter={submit} autoComplete="new-password" />
              {error && <div role="alert" style={{ color: "var(--danger, #cf1322)" }}>{error}</div>}
              <Button type="primary" loading={busy} onClick={submit}>创建</Button>
            </div>
          </Modal>
        </>
      )}
    </span>
  );
}
