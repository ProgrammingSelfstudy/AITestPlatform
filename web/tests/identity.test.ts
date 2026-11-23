import { afterEach, expect, test, vi } from "vitest";
import { USER_NAME_KEY, USER_NAME_TTL_MS, cleanName, forgetUserName, readUserName, rememberUserName, userNameExpiry } from "../src/identity";

const DAY = 24 * 60 * 60 * 1000;

afterEach(() => {
  window.localStorage.clear();
  vi.restoreAllMocks();
});

test("名字规则：去首尾空白，1 到 40 个字，不能有控制字符", () => {
  expect(cleanName("  小明 ")).toBe("小明");
  for (const bad of ["", "   ", "带\n换行", "a".repeat(41)]) expect(cleanName(bad), JSON.stringify(bad)).toBe("");
  expect(cleanName("长".repeat(40))).toBe("长".repeat(40));
});

test("填了名字记 30 天：能读回，到期时间是 30 天后；到期读取时自动清掉", () => {
  const now = 1_700_000_000_000;
  expect(rememberUserName("小明", now)).toBe(true);
  expect(readUserName(now + DAY)).toBe("小明");
  expect(userNameExpiry(now + DAY)).toBe(now + USER_NAME_TTL_MS);
  expect(USER_NAME_TTL_MS).toBe(30 * DAY);
  expect(readUserName(now + 30 * DAY + 1)).toBe("");
  expect(window.localStorage.getItem(USER_NAME_KEY)).toBeNull();
});

test("不合格的名字不记；同一个名字再填不续期；换名字重新记 30 天；清除后没有", () => {
  expect(rememberUserName("   ")).toBe(false);
  expect(window.localStorage.getItem(USER_NAME_KEY)).toBeNull();
  const first = 1_700_000_000_000;
  rememberUserName("小明", first);
  rememberUserName("小明", first + 10 * DAY);
  expect(userNameExpiry(first + 11 * DAY)).toBe(first + 30 * DAY);
  rememberUserName("小红", first + 10 * DAY);
  expect(readUserName(first + 11 * DAY)).toBe("小红");
  expect(userNameExpiry(first + 11 * DAY)).toBe(first + 40 * DAY);
  forgetUserName();
  expect(readUserName(first + 11 * DAY)).toBe("");
});

test("存储损坏或不可用时不抛错，当作没有", () => {
  window.localStorage.setItem(USER_NAME_KEY, "不是 json");
  expect(readUserName()).toBe("");
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new Error("存储被禁用");
  });
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
    throw new Error("存储被禁用");
  });
  expect(rememberUserName("小明")).toBe(false);
  expect(readUserName()).toBe("");
});
