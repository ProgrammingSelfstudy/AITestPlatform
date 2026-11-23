import { useCallback, useState } from "react";

/**
 * 个人令牌的「记住 30 天」。
 * - 令牌用来向服务端证明「这个节点是你的」（规则 9），用户选择在本浏览器里记一个月，免得每次刷新都重新贴。
 * - 存在 localStorage（只在这个浏览器、这个站点下），带到期时间，到期读取时自动清掉；随时可以「忘记」。
 * - 只记看起来完整的令牌（`u_` 开头加足够长），输到一半的不存；存取全部包在 try/catch 里：
 *   隐私模式或被禁用存储时退回「不记住」，页面照常用。
 * - 这只管个人令牌；审核口令（reviewLogic.ts）和 API Key 仍然不落本地存储。
 */

export const USER_TOKEN_KEY = "platform.userToken.v1";
export const USER_TOKEN_TTL_MS = 30 * 24 * 60 * 60 * 1000;

const LOOKS_COMPLETE = /^u_[A-Za-z0-9_-]{16,}$/;

interface Stored {
  token: string;
  expiresAt: number;
}

function readStored(now: number): Stored | null {
  try {
    const raw = window.localStorage.getItem(USER_TOKEN_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<Stored>;
    if (typeof parsed.token !== "string" || typeof parsed.expiresAt !== "number" || !LOOKS_COMPLETE.test(parsed.token) || parsed.expiresAt <= now) {
      window.localStorage.removeItem(USER_TOKEN_KEY);
      return null;
    }
    return { token: parsed.token, expiresAt: parsed.expiresAt };
  } catch {
    return null;
  }
}

export function readUserToken(now: number = Date.now()): string {
  return readStored(now)?.token ?? "";
}

/** 记住的令牌几号到期；没有记住（或已到期）返回 null。 */
export function userTokenExpiry(now: number = Date.now()): number | null {
  return readStored(now)?.expiresAt ?? null;
}

/** 看起来是完整令牌才记，返回是否记住了。已经记着同一个令牌就不续期（到期时间只由第一次填写决定）。 */
export function rememberUserToken(token: string, now: number = Date.now()): boolean {
  const value = token.trim();
  if (!LOOKS_COMPLETE.test(value)) return false;
  const current = readStored(now);
  if (current?.token === value) return true;
  try {
    window.localStorage.setItem(USER_TOKEN_KEY, JSON.stringify({ token: value, expiresAt: now + USER_TOKEN_TTL_MS } satisfies Stored));
    return true;
  } catch {
    return false;
  }
}

export function forgetUserToken(): void {
  try {
    window.localStorage.removeItem(USER_TOKEN_KEY);
  } catch {
    /* 存储不可用就当没有 */
  }
}

export function formatExpiry(expiresAt: number): string {
  const d = new Date(expiresAt);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** 页面里用：令牌状态 + 填写时自动记住 + 忘记。初始值来自已记住的令牌。 */
export function useRememberedUserToken() {
  const [token, setTokenState] = useState<string>(() => readUserToken());
  const [expiresAt, setExpiresAt] = useState<number | null>(() => userTokenExpiry());

  const setToken = useCallback((value: string) => {
    setTokenState(value);
    if (rememberUserToken(value)) setExpiresAt(userTokenExpiry());
  }, []);

  const forget = useCallback(() => {
    forgetUserToken();
    setTokenState("");
    setExpiresAt(null);
  }, []);

  return { token, setToken, expiresAt, forget };
}
