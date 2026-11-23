import { afterEach, expect, test, vi } from "vitest";
import { USER_TOKEN_KEY, USER_TOKEN_TTL_MS, forgetUserToken, readUserToken, rememberUserToken, userTokenExpiry } from "../src/userToken";

const FULL = `u_${"a1B2".repeat(16)}`;
const DAY = 24 * 60 * 60 * 1000;

afterEach(() => {
  window.localStorage.clear();
  vi.restoreAllMocks();
});

test("填完整的令牌记 30 天：能读回，到期时间是 30 天后", () => {
  const now = 1_700_000_000_000;
  expect(rememberUserToken(FULL, now)).toBe(true);
  expect(readUserToken(now + DAY)).toBe(FULL);
  expect(userTokenExpiry(now + DAY)).toBe(now + USER_TOKEN_TTL_MS);
  expect(USER_TOKEN_TTL_MS).toBe(30 * DAY);
});

test("到期后读取返回空并清掉，不会一直留着", () => {
  const now = 1_700_000_000_000;
  rememberUserToken(FULL, now);
  expect(readUserToken(now + 30 * DAY - 1)).toBe(FULL);
  expect(readUserToken(now + 30 * DAY + 1)).toBe("");
  expect(window.localStorage.getItem(USER_TOKEN_KEY)).toBeNull();
});

test("输到一半的、乱写的、带空格的不记", () => {
  for (const partial of ["", "u_", "u_short", "u_ab cd", `x_${"a".repeat(40)}`, "随便什么"]) {
    expect(rememberUserToken(partial), partial).toBe(false);
  }
  expect(window.localStorage.getItem(USER_TOKEN_KEY)).toBeNull();
});

test("同一个令牌再填不续期；换了令牌重新记 30 天；忘记后清空", () => {
  const first = 1_700_000_000_000;
  rememberUserToken(FULL, first);
  rememberUserToken(FULL, first + 10 * DAY);
  expect(userTokenExpiry(first + 11 * DAY)).toBe(first + 30 * DAY);
  const other = `u_${"z9Y8".repeat(16)}`;
  rememberUserToken(other, first + 10 * DAY);
  expect(readUserToken(first + 11 * DAY)).toBe(other);
  expect(userTokenExpiry(first + 11 * DAY)).toBe(first + 40 * DAY);
  forgetUserToken();
  expect(readUserToken(first + 11 * DAY)).toBe("");
});

test("被篡改或格式坏掉的存储当作没有；存储不可用时不抛错", () => {
  window.localStorage.setItem(USER_TOKEN_KEY, "不是 json");
  expect(readUserToken()).toBe("");
  window.localStorage.setItem(USER_TOKEN_KEY, JSON.stringify({ token: "u_short", expiresAt: Date.now() + DAY }));
  expect(readUserToken()).toBe("");
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new Error("存储被禁用");
  });
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
    throw new Error("存储被禁用");
  });
  expect(rememberUserToken(FULL)).toBe(false);
  expect(readUserToken()).toBe("");
});
