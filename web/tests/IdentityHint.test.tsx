import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import IdentityHint from "../src/IdentityHint";
import { readUserName, readUserToken } from "../src/identity";

const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => vi.stubGlobal("fetch", fetchMock));
afterEach(() => {
  vi.unstubAllGlobals();
  fetchMock.mockReset();
});

async function createName(name: string, password: string, onCreated: (name: string) => void) {
  const user = userEvent.setup();
  render(<IdentityHint userName="" expiresAt={null} onForget={() => {}} onCreated={onCreated} />);
  await user.click(screen.getByRole("button", { name: "创建名字" }));
  const dialog = within(await screen.findByRole("dialog", { name: "创建名字" }));
  await user.type(dialog.getByPlaceholderText("名字（1 到 40 个字）"), name);
  await user.type(dialog.getByLabelText("密码"), password);
  await user.click(dialog.getByRole("button", { name: /^创\s*建$/ }));
  return user;
}

const posts = () => fetchMock.mock.calls.filter(([, init]) => init?.method === "POST");

test("创建成功后自动登录，记住名字和会话", async () => {
  const name = "小明";
  const password = crypto.randomUUID();
  const token = "ab".repeat(32);
  const onCreated = vi.fn();
  fetchMock.mockImplementation(async (path) => {
    if (path === "/api/users") return Response.json({ name }, { status: 201 });
    if (path === "/api/login") return Response.json({ name, token });
    return Response.json([]);
  });

  await createName(name, password, onCreated);
  await waitFor(() => expect(onCreated).toHaveBeenCalledWith(name));
  expect(posts().map(([path]) => path)).toEqual(["/api/users", "/api/login"]);
  for (const [, init] of posts()) {
    const body = JSON.parse(String(init?.body));
    expect(body.name).toBe(name);
    expect(body.password === password).toBe(true);
  }
  expect(readUserName()).toBe(name);
  expect(readUserToken(name) === token).toBe(true);
  expect(Object.values(window.localStorage).some((value) => value.includes(password))).toBe(false);
});

test("创建成功但自动登录失败时，可以直接重试登录", async () => {
  const name = "小明";
  const password = crypto.randomUUID();
  const token = "ab".repeat(32);
  const onCreated = vi.fn();
  let attempts = 0;
  fetchMock.mockImplementation(async (path) => {
    if (path === "/api/users") return Response.json({ name }, { status: 201 });
    if (path === "/api/login") {
      attempts += 1;
      if (attempts === 1) return new Response("密码尝试次数过多，请等待 5 分钟后再试", { status: 429 });
      return Response.json({ name, token });
    }
    return Response.json([]);
  });

  const user = await createName(name, password, onCreated);
  const dialog = within(await screen.findByRole("dialog", { name: "登录" }));
  expect(dialog.getByRole("alert")).toHaveTextContent("名字已创建，自动登录失败");
  expect(dialog.getByLabelText("密码")).toHaveValue("");
  expect(dialog.queryByRole("button", { name: /^创\s*建$/ })).toBeNull();
  expect(onCreated).toHaveBeenCalledWith(name);
  expect(readUserToken(name)).toBe("");
  await user.type(dialog.getByLabelText("密码"), password);
  await user.click(dialog.getByRole("button", { name: /^登\s*录$/ }));
  await waitFor(() => expect(readUserToken(name) === token).toBe(true));
  expect(posts().map(([path]) => path)).toEqual(["/api/users", "/api/login", "/api/login"]);
});
