import { ConfigProvider, Empty, theme } from "antd";
import zhCN from "antd/locale/zh_CN";
import { useEffect, useState } from "react";
import NodesPage from "./NodesPage";
import RunPage from "./RunPage";
import { TaskPage, TasksPage } from "./TasksPage";
import "./styles.css";

type Mode = "light" | "dark";
type Route = { page: "nodes" | "tasks" | "missing" } | { page: "task" | "run"; id: string };

function readRoute(): Route {
  const hash = window.location.hash;
  if (!hash || hash === "#/nodes") return { page: "nodes" };
  if (hash === "#/tasks") return { page: "tasks" };
  const match = /^#\/(tasks|runs)\/([a-z0-9-]+)$/i.exec(hash);
  if (match) return { page: match[1] === "tasks" ? "task" : "run", id: match[2] };
  return { page: "missing" };
}

const THEME_KEY = "platform.theme";

// 主题偏好不是敏感信息，存本地；读写失败（隐私模式等）就退回跟随系统
function initialMode(): Mode {
  try {
    const saved = window.localStorage.getItem(THEME_KEY);
    if (saved === "light" || saved === "dark") return saved;
  } catch { /* 忽略 */ }
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

const BREADCRUMB: Record<Route["page"], string> = { nodes: "节点与设备", tasks: "任务", task: "任务详情", run: "运行详情", missing: "找不到这个页面" };

export default function App() {
  const [mode, setMode] = useState<Mode>(initialMode);
  const dark = mode === "dark";

  useEffect(() => {
    document.documentElement.dataset.theme = mode;
    try { window.localStorage.setItem(THEME_KEY, mode); } catch { /* 忽略 */ }
  }, [mode]);

  return (
    <ConfigProvider
      locale={zhCN}
      theme={{
        algorithm: dark ? theme.darkAlgorithm : theme.defaultAlgorithm,
        token: {
          colorPrimary: dark ? "#3b82f6" : "#2563eb",
          borderRadius: 8,
          colorBgContainer: dark ? "#111114" : "#ffffff",
          colorBgElevated: dark ? "#18181b" : "#ffffff",
          colorBgLayout: dark ? "#09090b" : "#fafafa",
        },
      }}
    >
      <Shell mode={mode} onToggle={() => setMode(dark ? "light" : "dark")} />
    </ConfigProvider>
  );
}

function Shell({ mode, onToggle }: { mode: Mode; onToggle: () => void }) {
  const [route, setRoute] = useState(readRoute);
  useEffect(() => {
    const change = () => setRoute(readRoute());
    window.addEventListener("hashchange", change);
    return () => window.removeEventListener("hashchange", change);
  }, []);
  const nodes = route.page === "nodes";
  const tasks = !nodes && route.page !== "missing";

  return (
    <div className="app-shell">
      <aside className="app-sidebar">
        <a className="app-brand" href="#/tasks">
          <span className="app-brand-mark" aria-hidden="true">AI</span>
          <span className="app-name">AI 测试平台</span>
        </a>
        <nav aria-label="主导航" className="app-nav">
          <div className="nav-label">核心工作台</div>
          <a href="#/nodes" aria-current={nodes ? "page" : undefined}>节点与设备</a>
          <a href="#/tasks" aria-current={tasks ? "page" : undefined}>任务</a>
        </nav>
      </aside>
      <div className="app-body">
        <header className="app-topbar">
          <div className="breadcrumb" aria-label="当前位置">
            {route.page === "task" || route.page === "run" ? <><a href="#/tasks">任务</a><span>/</span></> : null}
            <strong>{BREADCRUMB[route.page]}</strong>
          </div>
          <button type="button" className="theme-toggle" onClick={onToggle}>{mode === "dark" ? "浅色模式" : "深色模式"}</button>
        </header>
        <main className="app-main">
          {route.page === "nodes" && <NodesPage />}
          {route.page === "tasks" && <TasksPage />}
          {route.page === "task" && <TaskPage key={route.id} id={route.id} />}
          {route.page === "run" && <RunPage key={route.id} id={route.id} />}
          {route.page === "missing" && <Empty description="找不到这个页面" />}
        </main>
      </div>
    </div>
  );
}
