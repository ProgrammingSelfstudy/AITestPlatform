import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import ModuleCases from "../src/ModuleCases";
import ConfigImportDialog from "../src/ConfigImportDialog";

const fetchMock = vi.fn<typeof fetch>();
beforeEach(() => vi.stubGlobal("fetch", fetchMock));
afterEach(() => { vi.unstubAllGlobals(); fetchMock.mockReset(); });

/** 模拟刷新 / 关页面：返回是否被拦下（会弹浏览器的确认框） */
const blocked = () => {
  const event = new Event("beforeunload", { cancelable: true });
  window.dispatchEvent(event);
  return event.defaultPrevented;
};

test("导入 CSV 对话框：贴了内容还没导入时刷新会先提醒，关掉对话框或没内容时不拦", async () => {
  const user = userEvent.setup();
  render(<ModuleCases moduleId="m1" cases={[]} userName="小明" onChanged={() => {}} />);
  expect(blocked()).toBe(false);
  await user.click(screen.getByRole("button", { name: /导入 CSV/ }));
  const box = await screen.findByLabelText("CSV 内容");
  expect(blocked()).toBe(false);
  await user.click(box);
  await user.paste("编号,标题,步骤,预期");
  expect(blocked()).toBe(true);
  await user.click(screen.getByRole("button", { name: /取\s*消/ }));
  expect(blocked()).toBe(false);
});

test("导入配置对话框：同样，贴了内容还没导入时会提醒", async () => {
  const user = userEvent.setup();
  render(<ConfigImportDialog open userName="小明" onClose={() => {}} onDone={() => {}} />);
  expect(blocked()).toBe(false);
  await user.click(await screen.findByLabelText("配置 JSON"));
  await user.paste('{"types":[]}');
  expect(blocked()).toBe(true);
});
