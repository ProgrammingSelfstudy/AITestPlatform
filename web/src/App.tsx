import { Button, ConfigProvider, Empty, theme } from "antd";
import zhCN from "antd/locale/zh_CN";
import { useEffect, useState } from "react";
import NodesPage from "./NodesPage";
import ScriptsPage from "./ScriptsPage";
import AdminPage from "./AdminPage";
import RolesPage from "./RolesPage";
import ModulesPage from "./ModulesPage";
import ConfigsPage from "./ConfigsPage";
import ConfigDetailPage from "./ConfigDetailPage";
import ModuleDetailPage from "./ModuleDetailPage";
import RunPage from "./RunPage";
import { TaskPage, TasksPage } from "./TasksPage";
import "./styles.css";
import "./configs.css";

type Mode = "light" | "dark";
type Route = { page: "nodes" | "tasks" | "scripts" | "roles" | "modules" | "configs" | "admin" | "missing" } | { page: "task" | "run" | "module" | "config"; id: string };

function readRoute(): Route {
  const hash = window.location.hash;
  if (!hash || hash === "#/nodes") return { page: "nodes" };
  if (hash === "#/tasks") return { page: "tasks" };
  if (hash === "#/scripts") return { page: "scripts" };
  if (hash === "#/roles") return { page: "roles" };
  if (hash === "#/modules") return { page: "modules" };
  if (hash === "#/configs") return { page: "configs" };
  if (hash === "#/admin") return { page: "admin" };
  const configMatch = /^#\/configs\/([A-Za-z0-9_-]+)$/.exec(hash);
  if (configMatch) return { page: "config", id: configMatch[1] };
  const moduleMatch = /^#\/modules\/([a-z0-9-]+)$/i.exec(hash);
  if (moduleMatch) return { page: "module", id: moduleMatch[1] };
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

const BREADCRUMB: Record<Route["page"], string> = {
  nodes: "节点与设备",
  tasks: "测试任务",
  scripts: "回归脚本",
  roles: "角色与模型",
  modules: "模块",
  configs: "参数配置",
  config: "配置详情",
  admin: "管理员",
  module: "模块详情",
  task: "任务详情",
  run: "运行详情",
  missing: "找不到这个页面",
};

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
          colorBorder: dark ? "rgba(255, 255, 255, 0.10)" : "rgba(15, 23, 42, 0.10)",
          colorBorderSecondary: dark ? "rgba(255, 255, 255, 0.08)" : "rgba(15, 23, 42, 0.08)",
          colorText: dark ? "#fafafa" : "#09090b",
          colorTextSecondary: dark ? "#a1a1aa" : "#52525b",
          fontFamily: '-apple-system, BlinkMacSystemFont, "PingFang SC", "Hiragino Sans GB", "Inter", "Microsoft YaHei", sans-serif',
        },
        components: {
          Button: {
            primaryShadow: "none",
            defaultShadow: "none",
            defaultBg: dark ? "#111114" : "#ffffff",
            defaultBorderColor: dark ? "rgba(255, 255, 255, 0.10)" : "rgba(15, 23, 42, 0.10)",
          },
          Select: {
            optionSelectedBg: dark ? "rgba(59, 130, 246, 0.14)" : "rgba(37, 99, 235, 0.08)",
            optionSelectedColor: dark ? "#60a5fa" : "#2563eb",
            optionActiveBg: dark ? "#18181b" : "#f4f4f5",
          },
          Modal: {
            borderRadiusLG: 16,
            contentBg: dark ? "#111114" : "#ffffff",
            headerBg: dark ? "#111114" : "#ffffff",
          },
          Table: {
            headerBg: "transparent",
            headerSplitColor: "transparent",
            headerColor: dark ? "#8b8b94" : "#71717a",
            rowHoverBg: dark ? "#1c1c21" : "#f8fafc",
            borderColor: dark ? "rgba(255, 255, 255, 0.08)" : "rgba(15, 23, 42, 0.08)",
            cellPaddingBlock: 14,
            cellPaddingInline: 20,
          },
          Segmented: {
            trackBg: dark ? "#18181b" : "#f4f4f5",
            itemSelectedBg: dark ? "#111114" : "#ffffff",
          },
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
  const scripts = route.page === "scripts";
  const roles = route.page === "roles";
  const modules = route.page === "modules" || route.page === "module";
  const configs = route.page === "configs" || route.page === "config";
  const admin = route.page === "admin";
  const tasks = !nodes && !scripts && !roles && !modules && !configs && !admin && route.page !== "missing";

  return (
    <div className="app-shell">
      <aside className="app-sidebar">
        <a className="app-brand" href="#/tasks">
          <span className="app-brand-mark" aria-hidden="true">AI</span>
          <span className="app-name">AI 测试平台</span>
        </a>
        <div className="app-nav-group">
          <div className="nav-label">核心工作台</div>
          <nav aria-label="主导航" className="app-nav">
            <a href="#/nodes" aria-current={nodes ? "page" : undefined}>
              <svg className="nav-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <rect x="2" y="3" width="20" height="8" rx="2" />
                <rect x="2" y="13" width="20" height="8" rx="2" />
                <line x1="6" y1="7" x2="6.01" y2="7" />
                <line x1="6" y1="17" x2="6.01" y2="17" />
              </svg>
              <span>节点与设备</span>
            </a>
            <a href="#/admin" aria-current={admin ? "page" : undefined}>
              <svg className="nav-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <path d="M21 15a2 2 0 0 1-2 2H8l-5 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
              </svg>
              <span>管理员</span>
            </a>
            <a href="#/modules" aria-current={modules ? "page" : undefined}>
              <svg className="nav-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <rect x="3" y="3" width="7" height="7" rx="1" />
                <rect x="14" y="3" width="7" height="7" rx="1" />
                <rect x="3" y="14" width="7" height="7" rx="1" />
                <rect x="14" y="14" width="7" height="7" rx="1" />
              </svg>
              <span>模块</span>
            </a>
            <a href="#/tasks" aria-current={tasks ? "page" : undefined}>
              <svg className="nav-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <path d="M9 11l3 3L22 4" />
                <path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11" />
              </svg>
              <span>测试任务</span>
            </a>
            <a href="#/scripts" aria-current={scripts ? "page" : undefined}>
              <svg className="nav-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <polyline points="16 18 22 12 16 6" />
                <polyline points="8 6 2 12 8 18" />
              </svg>
              <span>回归脚本</span>
            </a>
            <a href="#/roles" aria-current={roles ? "page" : undefined}>
              <svg className="nav-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <circle cx="12" cy="8" r="4" />
                <path d="M4 21v-1a6 6 0 0 1 6-6h4a6 6 0 0 1 6 6v1" />
              </svg>
              <span>角色与模型</span>
            </a>
            <a href="#/configs" aria-current={configs ? "page" : undefined}>
              <svg className="nav-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <line x1="4" y1="6" x2="20" y2="6" />
                <line x1="4" y1="12" x2="20" y2="12" />
                <line x1="4" y1="18" x2="20" y2="18" />
                <circle cx="9" cy="6" r="2" fill="currentColor" />
                <circle cx="15" cy="12" r="2" fill="currentColor" />
                <circle cx="8" cy="18" r="2" fill="currentColor" />
              </svg>
              <span>参数配置</span>
            </a>
          </nav>
        </div>
        <div className="sidebar-note">
          <div className="sidebar-note-title">执行隔离规则</div>
          <div>任务只能在发起人自己的节点上执行，凭据仅在节点运行时注入。</div>
        </div>
      </aside>
      <div className="app-body">
        <header className="app-topbar">
          <div className="breadcrumb" aria-label="当前位置">
            {route.page === "task" || route.page === "run" ? (
              <>
                <a href="#/tasks">测试任务</a>
                <span className="breadcrumb-sep" aria-hidden="true">/</span>
                <strong>
                  {route.page === "task" ? "任务" : "运行"}{" "}
                  <span className="mono">{route.id.slice(0, 8)}</span>
                </strong>
              </>
            ) : (
              <>
                <span>工作区</span>
                <span className="breadcrumb-sep" aria-hidden="true">/</span>
                <strong>{BREADCRUMB[route.page]}</strong>
              </>
            )}
          </div>
          <div className="topbar-actions">
            <button type="button" className="theme-toggle" onClick={onToggle}>
              {mode === "dark" ? (
                <svg className="theme-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                  <circle cx="12" cy="12" r="4" />
                  <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41" />
                </svg>
              ) : (
                <svg className="theme-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                  <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
                </svg>
              )}
              <span>{mode === "dark" ? "浅色模式" : "深色模式"}</span>
            </button>
          </div>
        </header>
        <main className="app-main">
          {route.page === "nodes" && <NodesPage />}
          {route.page === "tasks" && <TasksPage />}
          {route.page === "scripts" && <ScriptsPage />}
          {route.page === "roles" && <RolesPage />}
          {route.page === "modules" && <ModulesPage />}
          {route.page === "configs" && <ConfigsPage />}
          {route.page === "config" && <ConfigDetailPage key={route.id} code={route.id} />}
          {route.page === "admin" && <AdminPage />}
          {route.page === "module" && <ModuleDetailPage key={route.id} id={route.id} />}
          {route.page === "task" && <TaskPage key={route.id} id={route.id} />}
          {route.page === "run" && <RunPage key={route.id} id={route.id} />}
          {route.page === "missing" && (
            <div className="card empty-state">
              <Empty description="找不到这个页面" />
              <div className="missing-actions">
                <Button type="primary" href="#/tasks">返回任务列表</Button>
              </div>
            </div>
          )}
        </main>
      </div>
    </div>
  );
}
