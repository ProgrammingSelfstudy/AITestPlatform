import { useCallback, useState } from "react";

/**
 * 「我是谁」：节点登记的无密码名字靠自觉，有密码的名字要登录。
 * - 名字和会话分别记 30 天，读取时清掉过期记录。
 * - 存取全部包在 try/catch 里，存储被禁用时会话只在当前页面的内存里保留。
 * - 请求头里中文不能直接放，统一用百分号编码（见 api.ts 的 userHeaders）。
 */

export const USER_NAME_KEY = "platform.userName.v1";
export const USER_NAME_TTL_MS = 30 * 24 * 60 * 60 * 1000;
export const MAX_NAME_CHARS = 40;
export const NEED_LOGIN_MESSAGE = "这个名字设了密码，请先登录";
const USER_SESSION_PREFIX = "platform.userSession.v1.";

interface Stored {
  name: string;
  expiresAt: number;
}

interface StoredSession {
  token: string;
  expiresAt: number;
}

const memorySessions = new Map<string, StoredSession>();

/** 名字规则（和服务端一致）：去首尾空白，1 到 40 个字，不能有控制字符。不合格返回空串。 */
export function cleanName(raw: string): string {
  const name = raw.replace(/^[\s\u0085]+|[\s\u0085]+$/g, "");
  // eslint-disable-next-line no-control-regex
  return name.length >= 1 && [...name].length <= MAX_NAME_CHARS && !/[\u0000-\u001f\u007f-\u009f]/.test(name) ? name : "";
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

export function readUserToken(raw: string, now: number = Date.now()): string {
  const name = cleanName(raw);
  if (!name) return "";
  try {
    const session: Partial<StoredSession> | null = memorySessions.get(name)
      ?? JSON.parse(window.localStorage.getItem(USER_SESSION_PREFIX + name) ?? "null");
    if (!session) return "";
    if (typeof session.token === "string" && typeof session.expiresAt === "number" && Number.isFinite(session.expiresAt) && session.expiresAt > now) {
      return session.token;
    }
  } catch {
    /* 存储不可读或记录损坏就当没有 */
  }
  forgetUserToken(name);
  return "";
}

export function rememberUserToken(raw: string, token: string, now: number = Date.now()): void {
  const name = cleanName(raw);
  if (!name || !token) return;
  const session = { token, expiresAt: now + USER_NAME_TTL_MS };
  try {
    window.localStorage.setItem(USER_SESSION_PREFIX + name, JSON.stringify(session));
    memorySessions.delete(name);
  } catch {
    memorySessions.set(name, session);
  }
}

export function forgetUserToken(raw: string): void {
  const name = cleanName(raw);
  if (!name) return;
  memorySessions.delete(name);
  try {
    window.localStorage.removeItem(USER_SESSION_PREFIX + name);
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
