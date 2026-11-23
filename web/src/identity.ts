import { useCallback, useState } from "react";

/**
 * 「我是谁」：只记名字，靠自觉。
 * - 名字不是密码、不是机密，只用来标明「谁在跑」「这是谁的节点」；任何人都能填别人的名字，系统不拦（用别人的额度靠大家自觉）。
 * - 填一次，在本浏览器记 30 天（localStorage，带到期时间，到期读取时自动清掉），随时可以清除。
 * - 存取全部包在 try/catch 里：隐私模式或被禁用存储时退回「不记住」，页面照常用。
 * - 请求头里中文不能直接放，统一用百分号编码（见 api.ts 的 userHeaders）。
 */

export const USER_NAME_KEY = "platform.userName.v1";
export const USER_NAME_TTL_MS = 30 * 24 * 60 * 60 * 1000;
export const MAX_NAME_CHARS = 40;

interface Stored {
  name: string;
  expiresAt: number;
}

/** 名字规则（和服务端一致）：去首尾空白，1 到 40 个字，不能有控制字符。不合格返回空串。 */
export function cleanName(raw: string): string {
  const name = raw.trim();
  // eslint-disable-next-line no-control-regex
  return name.length >= 1 && [...name].length <= MAX_NAME_CHARS && !/[\u0000-\u001f\u007f]/.test(name) ? name : "";
}

function readStored(now: number): Stored | null {
  try {
    const raw = window.localStorage.getItem(USER_NAME_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<Stored>;
    if (typeof parsed.name !== "string" || typeof parsed.expiresAt !== "number" || !cleanName(parsed.name) || parsed.expiresAt <= now) {
      window.localStorage.removeItem(USER_NAME_KEY);
      return null;
    }
    return { name: parsed.name, expiresAt: parsed.expiresAt };
  } catch {
    return null;
  }
}

export function readUserName(now: number = Date.now()): string {
  return readStored(now)?.name ?? "";
}

/** 记住的名字几号到期；没有记住（或已到期）返回 null。 */
export function userNameExpiry(now: number = Date.now()): number | null {
  return readStored(now)?.expiresAt ?? null;
}

/** 名字合格才记，返回是否记住了。同一个名字再填不续期（到期时间只由第一次填写决定）。 */
export function rememberUserName(raw: string, now: number = Date.now()): boolean {
  const name = cleanName(raw);
  if (!name) return false;
  if (readStored(now)?.name === name) return true;
  try {
    window.localStorage.setItem(USER_NAME_KEY, JSON.stringify({ name, expiresAt: now + USER_NAME_TTL_MS } satisfies Stored));
    return true;
  } catch {
    return false;
  }
}

export function forgetUserName(): void {
  try {
    window.localStorage.removeItem(USER_NAME_KEY);
  } catch {
    /* 存储不可用就当没有 */
  }
}

export function formatExpiry(expiresAt: number): string {
  const d = new Date(expiresAt);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** 页面里用：名字 + 填写时自动记住 + 清除。初始值来自已记住的名字。 */
export function useRememberedUserName() {
  const [name, setNameState] = useState<string>(() => readUserName());
  const [expiresAt, setExpiresAt] = useState<number | null>(() => userNameExpiry());

  const setName = useCallback((value: string) => {
    setNameState(value);
    if (rememberUserName(value)) setExpiresAt(userNameExpiry());
  }, []);

  const forget = useCallback(() => {
    forgetUserName();
    setNameState("");
    setExpiresAt(null);
  }, []);

  return { name, setName, expiresAt, forget };
}
