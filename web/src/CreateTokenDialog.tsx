import { Alert, Button, Input, Modal } from "antd";
import { useRef, useState } from "react";
import { createUser } from "./api";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";

/**
 * 创建个人令牌。令牌用来证明「这个节点是你的」（规则 9）：
 * - 创建需要审核口令，免得局域网里谁都能造身份；
 * - **令牌只在创建成功后显示这一次**（服务端库里只存摘要，丢了只能重新创建）；
 * - 创建后可以直接「使用这个令牌」，之后在本浏览器记 30 天。
 */
export default function CreateTokenDialog({ onClose, onUse }: { onClose: () => void; onUse: (token: string) => void }) {
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [needReviewToken, setNeedReviewToken] = useState(false);
  const [created, setCreated] = useState<{ name: string; token: string } | null>(null);
  const [copied, setCopied] = useState<"ok" | "fail" | null>(null);
  const lock = useRef(false);

  async function submit(entered?: string) {
    if (lock.current) return;
    const trimmed = name.trim();
    if (!trimmed) return setError("请填一个名字（给人看的，比如你的名字，不超过 40 个字）");
    if (trimmed.length > 40) return setError("名字不超过 40 个字");
    const review = entered ?? readReviewToken();
    const step = writeTokenAction(true, review);
    if (step === "prompt") {
      setNeedReviewToken(true);
      return;
    }
    if (entered !== undefined) rememberReviewToken(entered);
    lock.current = true;
    setNeedReviewToken(false);
    setSubmitting(true);
    setError(null);
    try {
      setCreated(await createUser(trimmed, review));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      lock.current = false;
      setSubmitting(false);
    }
  }

  async function copy(token: string) {
    try {
      await navigator.clipboard.writeText(token);
      setCopied("ok");
    } catch {
      setCopied("fail");
    }
  }

  if (created) {
    return (
      <Modal
        open
        title="个人令牌已创建"
        closable={false}
        mask={{ closable: false }}
        keyboard={false}
        footer={[
          <Button key="close" onClick={onClose}>先不用，关闭</Button>,
          <Button key="use" type="primary" onClick={() => { onUse(created.token); onClose(); }}>使用这个令牌</Button>,
        ]}
      >
        <Alert
          type="warning"
          showIcon
          title="令牌只显示这一次"
          description="服务端只保存它的摘要，关掉这个窗口就再也看不到了；丢了只能重新创建一个。"
        />
        <p style={{ margin: "12px 0 4px" }}>名字：{created.name}</p>
        <Input readOnly value={created.token} aria-label="新创建的个人令牌" onFocus={(e) => e.target.select()} style={{ fontFamily: "ui-monospace, Menlo, monospace" }} />
        <div style={{ margin: "8px 0", display: "flex", gap: 8, alignItems: "center" }}>
          <Button onClick={() => void copy(created.token)}>复制令牌</Button>
          {copied === "ok" && <span role="status">已复制</span>}
          {copied === "fail" && <span role="status">浏览器不让复制，请手动选中上面的令牌复制</span>}
        </div>
        <p style={{ margin: 0 }}>
          点「使用这个令牌」会把它填进页面，并在这个浏览器记 30 天。节点要属于你，启动节点时还要设置环境变量
          <code> NODE_USER_TOKEN </code>为这个令牌（节点第一次连上平台时据此绑定属主）。
        </p>
      </Modal>
    );
  }

  return (
    <>
      <Modal
        open
        title="创建个人令牌"
        okText={submitting ? "正在创建…" : "创建"}
        cancelText="取消"
        onOk={() => void submit()}
        onCancel={onClose}
        confirmLoading={submitting}
        okButtonProps={{ disabled: submitting }}
      >
        <p style={{ marginTop: 0 }}>个人令牌用来证明「这个节点是你的」。创建需要审核口令；创建后令牌只显示一次。</p>
        {error && <Alert type="error" showIcon title="创建失败" description={error} style={{ marginBottom: 12 }} />}
        <Input
          autoFocus
          aria-label="名字"
          placeholder="名字，比如你的名字（给人看的，不超过 40 个字）"
          value={name}
          maxLength={40}
          onChange={(e) => { setName(e.target.value); setError(null); }}
          onPressEnter={() => void submit()}
        />
      </Modal>
      {needReviewToken && <ReviewTokenDialog disabled={submitting} onConfirm={(t) => void submit(t)} onCancel={() => setNeedReviewToken(false)} />}
    </>
  );
}
