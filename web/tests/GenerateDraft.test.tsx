import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { clearDraft, loadDraft, saveDraft } from "../src/draftStore";
import GenerateDialog from "../src/GenerateDialog";

const fetchMock = vi.fn<typeof fetch>();
const PNG_DATA_URL = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==";

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  fetchMock.mockImplementation(async (input) => {
    const path = String(input);
    if (path === "/api/nodes") return Response.json([]);
    if (path === "/api/projects/hafa/modules") return Response.json([]);
    return new Response("没有这个接口", { status: 404 });
  });
});
afterEach(async () => {
  vi.unstubAllGlobals();
  fetchMock.mockReset();
  await clearDraft("generate-casegen");
});

const draft = (over: Record<string, unknown> = {}) => ({
  v: 1, moduleName: "语聊房", doc: "需求：房主可以邀请上麦", links: ["https://example.com/design"],
  shots: [{ id: "shot_3", name: "首页.png", size: 4096, previewUrl: PNG_DATA_URL }], ...over,
});

test("打开时恢复上次没提交的草稿：文字、链接、截图都在，并说明", async () => {
  await saveDraft("generate-casegen", draft());
  render(<GenerateDialog onClose={() => {}} />);
  expect(await screen.findByText(/已恢复上次没提交的草稿/)).toBeInTheDocument();
  expect(screen.getByDisplayValue("需求：房主可以邀请上麦")).toBeInTheDocument();
  expect(screen.getByDisplayValue("https://example.com/design")).toBeInTheDocument();
  expect(screen.getByText("首页.png")).toBeInTheDocument();
  expect(screen.getByText(/已上传 1 \/ 100 张/)).toBeInTheDocument();
});

test("有内容时会自动暂存（含截图），清空后草稿也清掉", async () => {
  const user = userEvent.setup();
  render(<GenerateDialog onClose={() => {}} />);
  const box = await screen.findByPlaceholderText(/直接粘贴需求文档内容/);
  await user.click(box);
  await user.paste("一段还没提交的需求");
  await waitFor(async () => expect((await loadDraft<{ doc: string }>("generate-casegen"))?.value.doc).toBe("一段还没提交的需求"), { timeout: 3000 });
  await user.clear(box);
  await waitFor(async () => expect(await loadDraft("generate-casegen")).toBeNull(), { timeout: 3000 });
});

test("清除草稿：内容清空，草稿删掉", async () => {
  await saveDraft("generate-casegen", draft());
  const user = userEvent.setup();
  render(<GenerateDialog onClose={() => {}} />);
  await user.click(await screen.findByRole("button", { name: /清除草稿/ }));
  expect(screen.queryByDisplayValue("需求：房主可以邀请上麦")).not.toBeInTheDocument();
  expect(screen.queryByText("首页.png")).not.toBeInTheDocument();
  await waitFor(async () => expect(await loadDraft("generate-casegen")).toBeNull(), { timeout: 3000 });
});

test("有没提交的内容时刷新页面会先提醒，没有内容时不拦", async () => {
  const user = userEvent.setup();
  const { unmount } = render(<GenerateDialog onClose={() => {}} />);
  const box = await screen.findByPlaceholderText(/直接粘贴需求文档内容/);
  const blocked = () => {
    const event = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(event);
    return event.defaultPrevented;
  };
  expect(blocked()).toBe(false);
  await user.click(box);
  await user.paste("还没提交");
  await waitFor(() => expect(blocked()).toBe(true));
  unmount();
  expect(blocked()).toBe(false);
});

test("草稿太旧（超过 30 天）读不出来", async () => {
  await saveDraft("generate-casegen", draft());
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(Date.now() + 31 * 24 * 60 * 60 * 1000);
  expect(await loadDraft("generate-casegen")).toBeNull();
  vi.useRealTimers();
});
