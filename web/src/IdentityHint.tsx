import { Button, Input, Modal, Select } from "antd";
import { useEffect, useState } from "react";
import { createUser, fetchNodes, type NodeView } from "./api";
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
  const [nodes, setNodes] = useState<NodeView[]>([]);
  const [claim, setClaim] = useState<string | undefined>();
  const [error, setError] = useState("");

  // 打开弹窗时取在线且没有主人的节点，供认领
  useEffect(() => {
    if (!open) return;
    const ctl = new AbortController();
    fetchNodes(ctl.signal).then((all) => setNodes(all.filter((n) => n.online && !n.owner))).catch(() => setNodes([]));
    return () => ctl.abort();
  }, [open]);

  const submit = async () => {
    const clean = cleanName(name);
    if (!clean) return setError("名字要 1 到 40 个字，不能有换行");
    if ([...password].length < 6) return setError("密码至少 6 个字符");
    setBusy(true);
    setError("");
    try {
      const created = await createUser(clean, password, claim);
      rememberUserName(created);
      onCreated?.(created);
      setOpen(false);
      setName("");
      setPassword("");
      setClaim(undefined);
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
              <Select
                allowClear
                placeholder={nodes.length ? "认领一个没有主人的在线节点（可不选）" : "现在没有可认领的节点（节点没设 NODE_USER_NAME 才没有主人）"}
                value={claim}
                onChange={setClaim}
                options={nodes.map((n) => ({ value: n.node_id, label: `${n.caps.os} · ${n.node_id.slice(0, 8)}` }))}
              />
              {error && <div role="alert" style={{ color: "var(--danger, #cf1322)" }}>{error}</div>}
              <Button type="primary" loading={busy} onClick={submit}>创建</Button>
            </div>
          </Modal>
        </>
      )}
    </span>
  );
}
