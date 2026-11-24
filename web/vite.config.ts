import react from "@vitejs/plugin-react";
import { loadEnv } from "vite";
import { defineConfig } from "vitest/config";

// 开发时把 /api 转给本机的平台服务端；没有鉴权之前，服务端只监听本机（见 docs/落地记录.md）
// 想指到别的地址：PLATFORM_HTTP=http://127.0.0.1:18080 pnpm dev
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, ".", "");
  return {
    plugins: [react()],
    // 前端测试：jsdom 里渲染组件，接口用假 fetch，不连平台服务端
    test: {
      environment: "jsdom",
      setupFiles: ["./tests/setup.ts"],
      include: ["tests/**/*.test.{ts,tsx}"],
      // antd 在 jsdom 里交互很慢，整套并行跑、机器忙时 5 秒默认值会偶发超时
      testTimeout: 15000,
      restoreMocks: true,
    },
    server: {
      // 默认 5273（5173 常被别的前端项目占用）。想换：PLATFORM_WEB_PORT=5300 pnpm dev。
      // 被占用就直接报错，不悄悄换到别的端口，免得你访问到别的项目
      port: Number(env.PLATFORM_WEB_PORT) || 5273,
      strictPort: true,
      // 局域网访问：PLATFORM_ALLOW_LAN=1 pnpm dev。默认只监听本机。没有登录鉴权，只在可信网络里开
      host: env.PLATFORM_ALLOW_LAN === "1" ? true : "127.0.0.1",
      proxy: {
        "/api": {
          target: env.PLATFORM_HTTP || "http://127.0.0.1:8080",
          // 写接口需要验证浏览器发来的 Host，代理保留原值。
          changeOrigin: false,
        },
      },
    },
  };
});
