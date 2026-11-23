import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { cancelRegression, decideScript, requestRegression } from "../src/api";
import { readReviewToken, rememberReviewToken } from "../src/reviewLogic";

const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  rememberReviewToken("");
});
afterEach(() => {
  vi.unstubAllGlobals();
  fetchMock.mockReset();
});

// 204 不能带正文（Response 构造会报错），其余用传入的正文；403/429 默认没有正文，和服务端的口令校验一致
const respond = (status: number, body = "") => fetchMock.mockResolvedValueOnce(new Response(status === 204 ? null : body, { status }));

function lastCall() {
  const [path, init] = fetchMock.mock.calls.at(-1)!;
  return { path: String(path), init: init as RequestInit, headers: new Headers((init as RequestInit).headers) };
}

test("发起回归：POST 带审核口令、个人令牌和写保护需要的头，请求体只有节点", async () => {
  respond(201, '{"id":"r1"}');
  await requestRegression("script-1", "node-1", "review-token-1", "u_personal-1");
  const { path, init, headers } = lastCall();
  expect(path).toBe("/api/scripts/script-1/regress");
  expect(init.method).toBe("POST");
  expect(init.redirect).toBe("error");
  expect(headers.get("content-type")).toBe("application/json");
  expect(headers.get("x-requested-with")).toBe("ai-test-platform");
  expect(headers.get("x-review-token")).toBe("review-token-1");
  expect(headers.get("x-user-token")).toBe("u_personal-1");
  expect(JSON.parse(String(init.body))).toEqual({ node_id: "node-1" });
});

test("取消回归：请求编号进路径，同样带两个令牌", async () => {
  respond(204);
  await cancelRegression("req/1", "review-token-1", "u_personal-1");
  const { path, headers } = lastCall();
  expect(path).toBe("/api/regression-requests/req%2F1/cancel");
  expect(headers.get("x-review-token")).toBe("review-token-1");
  expect(headers.get("x-user-token")).toBe("u_personal-1");
});

test("获准脚本：原因进请求体", async () => {
  respond(200, "{}");
  await decideScript("s1", "approve", "自检通过", "review-token-1");
  expect(lastCall().path).toBe("/api/scripts/s1/approve");
  expect(JSON.parse(String(lastCall().init.body))).toEqual({ note: "自检通过" });
});

test("审核口令不对（403 没有正文）：提示口令问题并清掉内存里的口令", async () => {
  rememberReviewToken("wrong-token");
  respond(403);
  await expect(requestRegression("s", "n", "wrong-token", "u_x")).rejects.toThrow("口令不对或写入已关闭");
  expect(readReviewToken()).toBe("");
});

test("403 带正文（例如不是自己的节点）：显示服务端原因，不清口令、不冒充口令问题", async () => {
  rememberReviewToken("right-token");
  respond(403, "只能使用自己的节点发起回归");
  await expect(requestRegression("s", "n", "right-token", "u_x")).rejects.toThrow("只能使用自己的节点发起回归");
  expect(readReviewToken()).toBe("right-token");
});

test("429 提示稍后再试且保留口令；其他错误显示服务端正文", async () => {
  rememberReviewToken("token-x");
  respond(429);
  await expect(requestRegression("s", "n", "token-x", "u_x")).rejects.toThrow("口令尝试次数过多");
  expect(readReviewToken()).toBe("token-x");
  respond(409, "这个脚本在该节点上已经有一个排队或运行中的回归请求");
  await expect(requestRegression("s", "n", "token-x", "u_x")).rejects.toThrow("已经有一个排队或运行中的回归请求");
  respond(500);
  await expect(requestRegression("s", "n", "token-x", "u_x")).rejects.toThrow("服务端返回 500");
});

test("连不上服务端", async () => {
  fetchMock.mockRejectedValueOnce(new TypeError("fetch failed"));
  await expect(requestRegression("s", "n", "token-x", "u_x")).rejects.toThrow("无法连接平台服务端");
});
