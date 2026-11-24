import { useCallback, useRef, useState } from "react";
import type { ReactNode } from "react";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";

type Job = (token: string) => Promise<void>;

/**
 * 需要「我是谁」+ 审核口令的写操作：没填名字先提示；没有口令就弹口令框，输对后记住；
 * 服务端的原因（口令不对、没登录、超限等）原样放进 `error`。返回的 `dialog` 要渲染在页面里。
 */
export function useGuardedWrite(userName: string) {
  const [pending, setPending] = useState<Job | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const lock = useRef(false);

  const execute = useCallback(
    async (job: Job, enteredToken?: string) => {
      if (lock.current) return;
      if (!userName.trim()) {
        setError("请先填「我是谁」");
        return;
      }
      const token = enteredToken ?? readReviewToken();
      const step = writeTokenAction(true, token);
      if (step === "prompt") {
        setPending(() => job);
        return;
      }
      if (enteredToken !== undefined) rememberReviewToken(enteredToken);
      lock.current = true;
      setPending(null);
      setBusy(true);
      setError(null);
      try {
        await job(token);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        lock.current = false;
        setBusy(false);
      }
    },
    [userName],
  );

  const dialog: ReactNode = pending ? (
    <ReviewTokenDialog disabled={busy} onConfirm={(token) => void execute(pending, token)} onCancel={() => setPending(null)} />
  ) : null;

  return { run: (job: Job) => execute(job), busy, error, setError, dialog };
}
