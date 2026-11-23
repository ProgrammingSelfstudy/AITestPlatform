import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { cancelRegression, decideScript, fetchNodeRoles, requestRegression } from "../src/api";
import { forgetUserToken, rememberUserToken, USER_NAME_TTL_MS } from "../src/identity";
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

test("发起回归：POST 带审核口令、「我是谁」（百分号编码）和写保护需要的头，请求体只有节点", async () => {
  respond(201, '{"id":"r1"}');
  await requestRegression("script-1", "node-1", "review-token-1", "alice");
  const { path, init, headers } = lastCall();
  expect(path).toBe("/api/scripts/script-1/regress");
  expect(init.method).toBe("POST");
  expect(init.redirect).toBe("error");
  expect(headers.get("content-type")).toBe("application/json");
  expect(headers.get("x-requested-with")).toBe("ai-test-platform");
  expect(headers.get("x-review-token")).toBe("review-token-1");
  expect(headers.get("x-user-name")).toBe("alice");
  expect(JSON.parse(String(init.body))).toEqual({ node_id: "node-1" });
});

test("取消回归：请求编号进路径，同样带审核口令和「我是谁」", async () => {
  respond(204);
  await cancelRegression("req/1", "review-token-1", "alice");
  const { path, headers } = lastCall();
  expect(path).toBe("/api/regression-requests/req%2F1/cancel");
  expect(headers.get("x-review-token")).toBe("review-token-1");
  expect(headers.get("x-user-name")).toBe("alice");
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

test("「我是谁」放在请求头里：中文做百分号编码，没填就不带", async () => {
  const calls: Headers[] = [];
  vi.stubGlobal("fetch", vi.fn(async (_url: unknown, init?: RequestInit) => {
    calls.push(new Headers(init?.headers));
    return Response.json({ online: true, roles: [], mine: true, name_problem: null });
  }));
  await fetchNodeRoles("n1", "小明");
  await fetchNodeRoles("n1", "  ");
  expect(calls[0].get("x-user-name")).toBe(encodeURIComponent("小明"));
  expect(calls[0].get("x-user-name")).toBe("%E5%B0%8F%E6%98%8E");
  expect(calls[1].get("x-user-name")).toBeNull();
  vi.unstubAllGlobals();
});

test("读写请求只给对应名字附带有效会话令牌", async () => {
  const token = "ab".repeat(32);
  rememberUserToken("小明", token);
  rememberUserToken("旧名字", token, Date.now() - USER_NAME_TTL_MS);

  respond(200, "{}");
  await fetchNodeRoles("n1", "\u0085 小明 ");
  expect(lastCall().headers.get("x-user-name")).toBe(encodeURIComponent("小明"));
  expect(lastCall().headers.get("x-user-token") === token).toBe(true);

  respond(201, '{"id":"r1"}');
  await requestRegression("s", "n", "", "小明");
  expect(lastCall().headers.get("x-user-token") === token).toBe(true);

  for (const name of ["小红", "旧名字", ""]) {
    respond(200, "{}");
    await fetchNodeRoles("n1", name);
    expect(lastCall().headers.has("x-user-token")).toBe(false);
  }
});

test("浏览器禁用存储时，当前页面仍能使用和清除会话", async () => {
  const name = "小明";
  const token = "ab".repeat(32);
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("存储被禁用"); });
  try {
    rememberUserToken(name, token);
    respond(200, "{}");
    await fetchNodeRoles("n1", name);
    expect(lastCall().headers.get("x-user-token") === token).toBe(true);
  } finally {
    forgetUserToken(name);
  }
  respond(200, "{}");
  await fetchNodeRoles("n1", name);
  expect(lastCall().headers.has("x-user-token")).toBe(false);
});
