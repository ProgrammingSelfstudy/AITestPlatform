import { useState } from "react";
import CreateTokenDialog from "./CreateTokenDialog";
import { formatExpiry } from "./userToken";

const linkStyle = { background: "none", border: "none", padding: 0, color: "var(--accent)", cursor: "pointer", font: "inherit", textDecoration: "underline" } as const;

/**
 * 个人令牌输入框旁边的说明：记着就写到期日并给「忘记」，没记着就说明填完整后会记 30 天；
 * 始终提供「创建个人令牌」入口，创建后可以直接使用。令牌本身永远不显示出来（创建成功那一次除外）。
 */
export default function UserTokenHint({ expiresAt, onForget, onToken, className }: { expiresAt: number | null; onForget: () => void; onToken: (token: string) => void; className?: string }) {
  const [creating, setCreating] = useState(false);
  return (
    <span className={className} role="status">
      {expiresAt ? (
        <>
          已记住，到期 {formatExpiry(expiresAt)}（只保存在这个浏览器）{" "}
          <button type="button" onClick={onForget} style={linkStyle}>忘记令牌</button>
        </>
      ) : (
        <>
          填完整后会在这个浏览器记住 30 天；没有令牌？{" "}
          <button type="button" onClick={() => setCreating(true)} style={linkStyle}>创建个人令牌</button>
        </>
      )}
      {creating && <CreateTokenDialog onClose={() => setCreating(false)} onUse={onToken} />}
    </span>
  );
}
