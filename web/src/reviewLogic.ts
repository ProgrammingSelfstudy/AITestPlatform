import type { Recording } from "./api";

type Timing = Pick<Recording, "seq" | "start_epoch_ms" | "duration_ms">;

export interface RecordingPosition {
  seq: number;
  seconds: number;
}

function bounds({ start_epoch_ms: start, duration_ms: duration }: Timing) {
  if (start == null || duration == null || !Number.isSafeInteger(start) || !Number.isSafeInteger(duration) || duration <= 0) {
    return null;
  }
  const end = start + duration;
  return Number.isSafeInteger(end) ? { start, end } : null;
}

export function findRecordingPosition(recordings: readonly Timing[], atMs?: number | null): RecordingPosition | null {
  if (atMs == null || !Number.isSafeInteger(atMs)) return null;
  const ordered = [...recordings].sort((left, right) => left.seq - right.seq);
  const intervals = ordered.map(bounds);
  for (const [index, interval] of intervals.entries()) {
    if (interval && atMs >= interval.start && atMs < interval.end) {
      return { seq: ordered[index].seq, seconds: (atMs - interval.start) / 1000 };
    }
  }
  for (let index = 1; index < intervals.length; index++) {
    const previous = intervals[index - 1];
    const next = intervals[index];
    if (previous && next && atMs >= previous.end && atMs < next.start) {
      return { seq: ordered[index].seq, seconds: 0 };
    }
  }
  return null;
}

const storageKeys = { token: "platform.review.token", reviewer: "platform.review.reviewer" };
type Field = keyof typeof storageKeys;
type StorageSource = () => Pick<Storage, "getItem" | "setItem" | "removeItem">;

export function readReviewValue(storage: StorageSource, field: Field): string {
  try {
    return storage().getItem(storageKeys[field]) ?? "";
  } catch {
    return "";
  }
}

export function writeReviewValue(storage: StorageSource, field: Field, value: string): void {
  try {
    const store = storage();
    if (value) {
      store.setItem(storageKeys[field], value);
    } else {
      try {
        store.removeItem(storageKeys[field]);
      } catch {
        store.setItem(storageKeys[field], "");
      }
    }
  } catch {
    return;
  }
}

export const browserStorage = () => window.localStorage;
// 口令只放内存，不写任何浏览器存储；刷新页面后需重新输入。
let sessionToken = "";

// 清掉旧版本遗留在 localStorage 里的口令明文
writeReviewValue(browserStorage, "token", "");

export function readReviewToken(): string {
  return sessionToken;
}

export function rememberReviewToken(token: string): void {
  sessionToken = token;
}

export function writeTokenAction(ready: boolean, token: string): "blocked" | "prompt" | "submit" {
  if (!ready) return "blocked";
  return token ? "submit" : "prompt";
}

export function writeTokenFailure(status: number) {
  if (status === 403) return { clearToken: true, message: "口令不对或写入已关闭" };
  if (status === 429) return { clearToken: false, message: "口令尝试次数过多，请稍后再试" };
  return null;
}
