import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// 每个测试后卸载渲染出来的组件，免得上一个测试的页面留在 document 里串到下一个
afterEach(() => {
  cleanup();
  // 「我是谁」会记在 localStorage 里，不清掉的话上一个测试填的名字会带进下一个测试
  window.localStorage.clear();
});

// antd 用到 matchMedia 和 ResizeObserver，jsdom 没有；给最小的空实现
if (!window.matchMedia) {
  window.matchMedia = (query: string) =>
    ({ matches: false, media: query, onchange: null, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, dispatchEvent: () => false }) as MediaQueryList;
}
if (!("ResizeObserver" in window)) {
  (window as unknown as { ResizeObserver: unknown }).ResizeObserver = class { observe() {} unobserve() {} disconnect() {} };
}
