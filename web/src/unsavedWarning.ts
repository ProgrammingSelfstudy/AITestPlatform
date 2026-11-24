import { useEffect } from "react";

/**
 * 有没提交的内容时，刷新或关闭页面前让浏览器先问一声（浏览器自己的确认框，文字由浏览器决定）。
 * `dirty` 为真才拦；提交成功、清掉内容后要变回 false。
 */
export function useUnsavedWarning(dirty: boolean): void {
  useEffect(() => {
    if (!dirty) return;
    const handler = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      // 部分浏览器要求设置 returnValue 才会弹确认框
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", handler);
    return () => window.removeEventListener("beforeunload", handler);
  }, [dirty]);
}
