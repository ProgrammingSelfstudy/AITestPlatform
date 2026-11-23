import { formatExpiry } from "./identity";

const linkStyle = { background: "none", border: "none", padding: 0, color: "var(--accent)", cursor: "pointer", font: "inherit", textDecoration: "underline" } as const;

/**
 * 「我是谁」输入框旁边的说明：只记名字、靠自觉；记着就写到期日并给「清除」。
 * 名字不是机密——它只是标明谁在跑；各人用自己节点的额度。
 */
export default function IdentityHint({ expiresAt, onForget, className }: { expiresAt: number | null; onForget: () => void; className?: string }) {
  return (
    <span className={className} role="status">
      {expiresAt ? (
        <>
          已记住，到期 {formatExpiry(expiresAt)}（只保存在这个浏览器）{" "}
          <button type="button" onClick={onForget} style={linkStyle}>清除</button>
        </>
      ) : (
        "填你的名字，和启动节点时设的 NODE_USER_NAME 一致；会在这个浏览器记 30 天"
      )}
    </span>
  );
}
