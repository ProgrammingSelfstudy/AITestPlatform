import { Alert, Input, Modal } from "antd";
import { useState } from "react";

interface Props {
  onConfirm: (token: string) => void;
  onCancel: () => void;
  disabled?: boolean;
}

export default function ReviewTokenDialog({ onConfirm, onCancel, disabled }: Props) {
  const [token, setToken] = useState("");
  const [missing, setMissing] = useState(false);

  function confirm() {
    if (disabled) return;
    if (!token) {
      setMissing(true);
      return;
    }
    onConfirm(token);
  }

  return (
    <Modal open title="输入审核口令" okText="确认" cancelText="取消" width={520}
      onOk={confirm} onCancel={onCancel} okButtonProps={{ disabled }}>
      <div className="review-form">
        {missing && <Alert type="error" title="请输入审核口令" showIcon />}
        <Input.Password aria-label="审核口令" placeholder="请输入审核口令" autoComplete="off" autoFocus
          value={token} onChange={(event) => setToken(event.target.value)} onPressEnter={confirm} />
      </div>
    </Modal>
  );
}
