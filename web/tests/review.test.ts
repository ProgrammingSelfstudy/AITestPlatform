import { expect, test } from "vitest";
import { findRecordingPosition, readReviewValue, writeReviewValue, writeTokenAction, writeTokenFailure } from "../src/reviewLogic";

const first = { seq: 1, start_epoch_ms: 1000, duration_ms: 500 };
const second = { seq: 2, start_epoch_ms: 1700, duration_ms: 600 };

test("步骤按分段时间定位，起点包含且终点不包含", () => {
  const recordings = [second, first];
  expect(findRecordingPosition(recordings, 1000)).toEqual({ seq: 1, seconds: 0 });
  expect(findRecordingPosition(recordings, 1255)).toEqual({ seq: 1, seconds: 0.255 });
  expect(findRecordingPosition(recordings, 1700)).toEqual({ seq: 2, seconds: 0 });
  expect(findRecordingPosition(recordings, 2299)).toEqual({ seq: 2, seconds: 0.599 });
  expect(findRecordingPosition(recordings, 2300)).toBe(null);
});

test("段间空档跳到下一段开头，未覆盖区间和旧记录不可定位", () => {
  const recordings = [first, second];
  expect(findRecordingPosition(recordings, 1500)).toEqual({ seq: 2, seconds: 0 });
  expect(findRecordingPosition(recordings, 1699)).toEqual({ seq: 2, seconds: 0 });
  for (const timestamp of [999, 2400, null, undefined, NaN]) {
    expect(findRecordingPosition(recordings, timestamp)).toBe(null);
  }
  expect(findRecordingPosition([], 1200)).toBe(null);
  expect(findRecordingPosition([{ ...first, duration_ms: null }, second], 1500)).toBe(null);
  expect(findRecordingPosition([{ ...first, start_epoch_ms: null }], 1200)).toBe(null);
});

test("审核口令原样存取和清除，与审核人姓名分开", () => {
  const entries = new Map<string, string>();
  const storage = () => ({
    getItem: (key: string) => entries.get(key) ?? null,
    setItem: (key: string, value: string) => entries.set(key, value),
    removeItem: (key: string) => entries.delete(key),
  });
  expect(readReviewValue(storage, "token")).toBe("");
  writeReviewValue(storage, "token", " synthetic-review-token ");
  writeReviewValue(storage, "reviewer", "测试员");
  expect(readReviewValue(storage, "token")).toBe(" synthetic-review-token ");
  expect(readReviewValue(storage, "reviewer")).toBe("测试员");
  writeReviewValue(storage, "token", "");
  expect(readReviewValue(storage, "token")).toBe("");
  expect(readReviewValue(storage, "reviewer")).toBe("测试员");
  expect(entries.size).toBe(1);
});

test("浏览器禁止访问、读取、写入或删除本地存储时不抛异常", () => {
  const blocked = () => { throw new Error("存储不可用"); };
  const partial = () => ({ getItem: blocked, setItem: blocked, removeItem: blocked });
  for (const storage of [blocked, partial]) {
    expect(readReviewValue(storage, "token")).toBe("");
    expect(() => writeReviewValue(storage, "token", "synthetic-review-token")).not.toThrow();
    expect(() => writeReviewValue(storage, "token", "")).not.toThrow();
  }
  let stored = "synthetic-review-token";
  const removeBlocked = () => ({
    getItem: () => stored,
    setItem: (_key: string, value: string) => { stored = value; },
    removeItem: blocked,
  });
  writeReviewValue(removeBlocked, "token", "");
  expect(readReviewValue(removeBlocked, "token")).toBe("");
});

test("只有准备提交写入且没有口令时才弹出口令框", () => {
  expect(writeTokenAction(false, "")).toBe("blocked");
  expect(writeTokenAction(false, "synthetic-review-token")).toBe("blocked");
  expect(writeTokenAction(true, "")).toBe("prompt");
  expect(writeTokenAction(true, "synthetic-review-token")).toBe("submit");
  expect(writeTokenAction(true, " ")).toBe("submit");
});

test("403 清除口令并提示写入未授权，429 保留口令并提示稍后再试", () => {
  expect(writeTokenFailure(403)).toEqual({ clearToken: true, message: "口令不对或写入已关闭" });
  expect(writeTokenFailure(429)).toEqual({ clearToken: false, message: "口令尝试次数过多，请稍后再试" });
  for (const status of [201, 204, 400, 404, 409, 500]) {
    expect(writeTokenFailure(status)).toBe(null);
  }
});
