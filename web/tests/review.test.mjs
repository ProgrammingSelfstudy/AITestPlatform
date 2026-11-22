import assert from "node:assert/strict";
import test from "node:test";
import { findRecordingPosition, readReviewValue, writeReviewValue, writeTokenAction, writeTokenFailure } from "../src/reviewLogic.ts";

const first = { seq: 1, start_epoch_ms: 1000, duration_ms: 500 };
const second = { seq: 2, start_epoch_ms: 1700, duration_ms: 600 };

test("步骤按分段时间定位，起点包含且终点不包含", () => {
  const recordings = [second, first];
  assert.deepEqual(findRecordingPosition(recordings, 1000), { seq: 1, seconds: 0 });
  assert.deepEqual(findRecordingPosition(recordings, 1255), { seq: 1, seconds: 0.255 });
  assert.deepEqual(findRecordingPosition(recordings, 1700), { seq: 2, seconds: 0 });
  assert.deepEqual(findRecordingPosition(recordings, 2299), { seq: 2, seconds: 0.599 });
  assert.equal(findRecordingPosition(recordings, 2300), null);
});

test("段间空档跳到下一段开头，未覆盖区间和旧记录不可定位", () => {
  const recordings = [first, second];
  assert.deepEqual(findRecordingPosition(recordings, 1500), { seq: 2, seconds: 0 });
  assert.deepEqual(findRecordingPosition(recordings, 1699), { seq: 2, seconds: 0 });
  for (const timestamp of [999, 2400, null, undefined, NaN]) {
    assert.equal(findRecordingPosition(recordings, timestamp), null);
  }
  assert.equal(findRecordingPosition([], 1200), null);
  assert.equal(findRecordingPosition([{ ...first, duration_ms: null }, second], 1500), null);
  assert.equal(findRecordingPosition([{ ...first, start_epoch_ms: null }], 1200), null);
});

test("审核口令原样存取和清除，与审核人姓名分开", () => {
  const entries = new Map();
  const storage = () => ({
    getItem: (key) => entries.get(key) ?? null,
    setItem: (key, value) => entries.set(key, value),
    removeItem: (key) => entries.delete(key),
  });
  assert.equal(readReviewValue(storage, "token"), "");
  writeReviewValue(storage, "token", " synthetic-review-token ");
  writeReviewValue(storage, "reviewer", "测试员");
  assert.equal(readReviewValue(storage, "token"), " synthetic-review-token ");
  assert.equal(readReviewValue(storage, "reviewer"), "测试员");
  writeReviewValue(storage, "token", "");
  assert.equal(readReviewValue(storage, "token"), "");
  assert.equal(readReviewValue(storage, "reviewer"), "测试员");
  assert.equal(entries.size, 1);
});

test("浏览器禁止访问、读取、写入或删除本地存储时不抛异常", () => {
  const blocked = () => { throw new Error("存储不可用"); };
  const partial = () => ({ getItem: blocked, setItem: blocked, removeItem: blocked });
  for (const storage of [blocked, partial]) {
    assert.equal(readReviewValue(storage, "token"), "");
    assert.doesNotThrow(() => writeReviewValue(storage, "token", "synthetic-review-token"));
    assert.doesNotThrow(() => writeReviewValue(storage, "token", ""));
  }
  let stored = "synthetic-review-token";
  const removeBlocked = () => ({
    getItem: () => stored,
    setItem: (_key, value) => { stored = value; },
    removeItem: blocked,
  });
  writeReviewValue(removeBlocked, "token", "");
  assert.equal(readReviewValue(removeBlocked, "token"), "");
});

test("只有准备提交写入且没有口令时才弹出口令框", () => {
  assert.equal(writeTokenAction(false, ""), "blocked");
  assert.equal(writeTokenAction(false, "synthetic-review-token"), "blocked");
  assert.equal(writeTokenAction(true, ""), "prompt");
  assert.equal(writeTokenAction(true, "synthetic-review-token"), "submit");
  assert.equal(writeTokenAction(true, " "), "submit");
});

test("403 清除口令并提示写入未授权，429 保留口令并提示稍后再试", () => {
  assert.deepEqual(writeTokenFailure(403), { clearToken: true, message: "口令不对或写入已关闭" });
  assert.deepEqual(writeTokenFailure(429), { clearToken: false, message: "口令尝试次数过多，请稍后再试" });
  for (const status of [201, 204, 400, 404, 409, 500]) {
    assert.equal(writeTokenFailure(status), null);
  }
});
