import { Button } from "antd";
import { useCallback, useEffect, useRef, useState } from "react";
import AppIcon from "./AppIcons";
import { fetchNodeApps, scanNodeApp, type AppView, type ModelRole, type NodeApps, type RoleView } from "./api";
import { usePolling } from "./usePolling";
import "./apps.css";

/**
 * 「选应用 → 检查本机环境 → 选模型创建角色」面板。
 * - 数据是公开的：谁都能看到每个应用上一次的检查结果；只有节点主人能触发检查、创建角色（花他自己的额度、改他自己的节点）；
 * - 检查只跑本地命令（版本、登录状态、模型列表），不调模型、不花额度，所以不要审核口令；
 * - 创建角色只产出 RoleInput，由父页面和现有角色一起整体提交（那里走审核口令流程）；
 * - 「我是谁」只是名字，不是机密，只用来让服务端知道你是不是这个节点的主人。
 */

type Slot = ModelRole["slot"];

interface AppMeta {
  source: string;
  name: string;
  tagline: string;
  /** 这个应用的模型能担任哪些岗位（和服务端校验一致） */
  slots: Slot[];
  install: string;
}

const APPS: AppMeta[] = [
  { source: "claude_cli", name: "Claude", tagline: "Claude Code 命令行 · 会员登录", slots: ["judge", "case_writer", "planner"], install: "安装 Claude Code 后在终端运行 claude auth login，用会员账号登录" },
  { source: "codex_exec", name: "Codex", tagline: "Codex CLI · ChatGPT 登录", slots: ["executor", "case_writer", "planner"], install: "安装 Codex CLI 后在终端运行 codex login，用 ChatGPT 账号登录" },
  { source: "agy", name: "Gemini", tagline: "Antigravity CLI · 个人账号登录", slots: ["judge", "planner"], install: "安装 Antigravity CLI 后在终端运行 agy，用个人 Google 账号登录" },
  { source: "deepseek", name: "DeepSeek", tagline: "DeepSeek · API Key", slots: [], install: "" },
];

/** 新建角色时「角色名称」的默认值（写死，可手动改） */
const DEFAULT_ROLE_NAME = "研发流程测试";

type Tone = "ok" | "warn" | "bad" | "muted";

function statusOf(meta: AppMeta, view: AppView | undefined): { text: string; tone: Tone } {
  if (meta.source === "deepseek") return { text: "开发中", tone: "muted" };
  if (!view?.scanned) return { text: "未检查", tone: "muted" };
  if (!view.installed) return { text: "未安装", tone: "warn" };
  if (view.login === "ok") return { text: "检测到并可用", tone: "ok" };
  if (view.login === "wrong_auth") return { text: "登录方式不对", tone: "bad" };
  return { text: "未登录", tone: "warn" };
}

/** 带到「新增角色」编辑区的预填内容 */
export interface AppRoleDraft {
  source: string;
  slot: Slot;
  name: string;
  models: { model: string; efforts: string[] }[];
  default: { model: string; effort: string | null };
}

export interface AppsPanelProps {
  nodeId: string;
  /** 「我是谁」（名字，可为空）。只用来让服务端告诉页面这个节点是不是你的 */
  userName: string;
  online: boolean;
  existingRoles: RoleView[];
  busy: boolean;
  onDraftRole: (draft: AppRoleDraft) => void;
  /** 有值表示你现在只能看：卡片照常显示、能看到上次的检查结果，但不能触发检查、不能创建角色，并显示这句原因 */
  readOnlyReason?: string;
}

interface ScanState {
  source: string;
  /** 点击时这个应用上一次检查的时间；变了就说明节点回报了新结果 */
  previous: string | null;
  error: string | null;
  timedOut: boolean;
}

const SCAN_TIMEOUT_MS = 30_000;

export default function AppsPanel({ nodeId, userName, online, existingRoles, busy, onDraftRole, readOnlyReason }: AppsPanelProps) {
  const canAct = online && !readOnlyReason;
  const [selected, setSelected] = useState<string | null>(null);
  const [scan, setScan] = useState<ScanState | null>(null);

  // 轮询节点上的检查结果；数据没变就复用旧引用，页面不会闪
  const lastRef = useRef<{ key: string; value: NodeApps } | null>(null);
  const load = useCallback(
    async (signal: AbortSignal) => {
      // 还没选节点时不请求；其余情况谁都能看（公开）
      if (!nodeId) return { online: false, apps: [], mine: null, name_problem: null } satisfies NodeApps;
      const next = await fetchNodeApps(nodeId, userName, signal);
      const key = JSON.stringify(next);
      if (lastRef.current?.key === key) return lastRef.current.value;
      lastRef.current = { key, value: next };
      return next;
    },
    [nodeId, userName],
  );
  const { data, error, reload } = usePolling(load, 1000);

  const viewOf = (source: string) => data?.apps.find((a) => a.source === source);
  const selectedMeta = APPS.find((a) => a.source === selected) ?? null;
  const selectedView = selected ? viewOf(selected) : undefined;
  const scanning = Boolean(scan && selected === scan.source && !scan.error && !scan.timedOut && (selectedView?.scanned_at ?? null) === scan.previous);

  const startScan = useCallback(
    (source: string) => {
      const previous = data?.apps.find((a) => a.source === source)?.scanned_at ?? null;
      setScan({ source, previous, error: null, timedOut: false });
      // 请求发出后马上刷新一次，不用干等下一轮轮询
      scanNodeApp(nodeId, source, userName)
        .then(() => reload())
        .catch((err: unknown) => {
          setScan((current) => (current && current.source === source ? { ...current, error: err instanceof Error ? err.message : String(err) } : current));
        });
    },
    [data, nodeId, reload],
  );

  function choose(meta: AppMeta) {
    if (meta.source === "deepseek" || !nodeId) return;
    setSelected(meta.source);
    // 只有节点主人才触发检查；别人点开只看上一次的结果
    if (canAct) startScan(meta.source);
  }

  // 30 秒还没等到新结果：不再转圈，提示节点没有响应
  useEffect(() => {
    if (!scanning || !scan) return;
    const timer = window.setTimeout(() => setScan((current) => (current ? { ...current, timedOut: true } : current)), SCAN_TIMEOUT_MS);
    return () => window.clearTimeout(timer);
  }, [scanning, scan]);

  return (
    <section className="apps-panel" aria-label="选择应用并配置角色">
      <header className="apps-head">
        <div>
          <span className="apps-kicker">第 1 步</span>
          <h2 className="apps-title">选择应用</h2>
          <p className="apps-sub">点一个应用，先检查你这台机器上有没有它、怎么登录的、有哪些模型，再用这些模型创建角色。</p>
        </div>
        <span className="apps-count">节点上现在有 {existingRoles.length} 个角色</span>
      </header>

      {error && <div className="apps-banner is-bad" role="alert">读取应用检查结果失败：{error}</div>}
      {readOnlyReason ? (
        <div className="apps-banner is-warn" role="status">{readOnlyReason}</div>
      ) : (
        !online && <div className="apps-banner is-warn">节点离线，无法检查本机环境。节点恢复在线后再试。</div>
      )}

      <div className="apps-cards">
        {APPS.map((meta) => {
          const status = statusOf(meta, viewOf(meta.source));
          // 只读时卡片仍可点开看上一次的结果，所以不禁用；DeepSeek 没有运行器点不进去
          const disabled = meta.source === "deepseek" || !nodeId;
          return (
            <button
              key={meta.source}
              type="button"
              className={`apps-card${selected === meta.source ? " is-selected" : ""}${disabled ? " is-disabled" : ""}`}
              aria-pressed={selected === meta.source}
              aria-disabled={disabled}
              onClick={() => choose(meta)}
            >
              <span className="apps-card-art">
                <AppIcon source={meta.source} size={128} />
              </span>
              <span className="apps-card-name">{meta.name}</span>
              <span className="apps-card-tag">{meta.tagline}</span>
              <span className={`apps-chip is-${status.tone}`}>{status.text}</span>
              {meta.source === "deepseek" && <span className="apps-card-note">运行器还在开发中，暂时不能配置</span>}
            </button>
          );
        })}
      </div>

      {selectedMeta && (
        <AppDetail
          canAct={canAct}
          key={selectedMeta.source}
          meta={selectedMeta}
          view={selectedView}
          scanning={scanning}
          scan={scan && scan.source === selectedMeta.source ? scan : null}
          onRescan={() => startScan(selectedMeta.source)}
          busy={busy}
          onDraftRole={onDraftRole}
        />
      )}
    </section>
  );
}

// ---------------------------------------------------------------- 第 2、3 步：环境检查结果与创建角色

interface DetailProps {
  canAct: boolean;
  meta: AppMeta;
  view: AppView | undefined;
  scanning: boolean;
  scan: ScanState | null;
  onRescan: () => void;
  busy: boolean;
  onDraftRole: (draft: AppRoleDraft) => void;
}

function Check({ pass, label, detail }: { pass: boolean; label: string; detail: string }) {
  return (
    <li className={`apps-check ${pass ? "is-pass" : "is-fail"}`}>
      <span className="apps-check-mark" aria-hidden="true">{pass ? "✓" : "✗"}</span>
      <span className="apps-check-body">
        <strong>
          {label}：<span className="apps-check-verdict">{pass ? "通过" : "未通过"}</span>
        </strong>
        <span className="apps-check-detail">{detail}</span>
      </span>
    </li>
  );
}

function AppDetail({ canAct, meta, view, scanning, scan, onRescan, busy, onDraftRole }: DetailProps) {
  const ready = Boolean(view?.scanned && view.installed && view.login === "ok");
  const failure = scan?.error ?? (scan?.timedOut ? "节点没有响应，请确认节点还在运行，然后点「重新检查」" : null);

  return (
    <div className="apps-detail" aria-live="polite">
      <header className="apps-detail-head">
        <div>
          <span className="apps-kicker">第 2 步</span>
          <h3 className="apps-detail-title">检查本机环境 · {meta.name}</h3>
          <p className="apps-sub">只跑本地命令（版本、登录状态、模型列表），不调用模型、不花额度。</p>
        </div>
        {canAct && <Button onClick={onRescan} disabled={scanning}>{scanning ? "检查中…" : "重新检查"}</Button>}
      </header>

      {failure && <div className="apps-banner is-bad" role="alert">{failure}</div>}

      {scanning && !failure && (
        <div className="apps-scanning" role="status">
          <span className="apps-dot" aria-hidden="true" />
          正在让节点检查本机的 {meta.name}…
        </div>
      )}

      {!view?.scanned && !scanning && !canAct && (
        <div className="apps-banner">这个节点还没有检查过本机的 {meta.name}。只有节点主人能触发检查。</div>
      )}

      {view?.scanned && !scanning && (
        <>
          <ul className="apps-checklist">
            <Check
              pass={Boolean(view.installed)}
              label="已安装"
              detail={view.installed ? `版本 ${view.version ?? "未知"}` : `${view.login_detail ?? ""}${meta.install ? `（${meta.install}）` : ""}`}
            />
            {view.installed && (
              <Check
                pass={view.login === "ok"}
                label="登录方式"
                detail={`${view.login_detail ?? ""}${view.login === "wrong_auth" ? "。平台只接受会员或个人账号登录，不使用 API Key" : view.login === "not_logged_in" && meta.install ? `（${meta.install}）` : ""}`}
              />
            )}
            {view.installed && (
              <Check
                pass={(view.models?.length ?? 0) > 0}
                label="模型"
                detail={`${view.models_note ?? ""}${(view.models?.length ?? 0) > 0 ? `（取到 ${view.models?.length} 个）` : ""}`}
              />
            )}
          </ul>

          {ready && !canAct ? (
            <div className="apps-banner">这是上一次的检查结果。只有节点主人能用这些模型创建角色。</div>
          ) : ready ? (
            <CreateRoles meta={meta} view={view} busy={busy} onDraftRole={onDraftRole} />
          ) : (
            <div className="apps-banner is-warn">
              {!view.installed ? `本机没有找到 ${meta.name}，不能继续。` : view.login === "wrong_auth" ? "登录方式不对，不能继续。" : "还没有登录，不能继续。"}按上面的提示处理后点「重新检查」。
            </div>
          )}
        </>
      )}
    </div>
  );
}

/** 环境检查通过后：不在这里填表，只把来源和检查到的模型带到下面「新增角色」编辑区（和角色列表同一层）去补全、保存 */
function CreateRoles({ meta, view, onDraftRole, busy }: Pick<DetailProps, "meta" | "busy" | "onDraftRole"> & { view: AppView }) {
  function draft() {
    const models = view.models ?? [];
    const first = models[0];
    onDraftRole({
      source: meta.source,
      slot: meta.slots[0] ?? "judge",
      name: DEFAULT_ROLE_NAME,
      models: models.map((m) => ({ model: m.id, efforts: m.efforts })),
      default: { model: first?.id ?? "", effort: first?.default_effort ?? null },
    });
  }
  return (
    <div className="apps-actions">
      <span className="apps-hint">环境检查通过。点右边按钮，把 {meta.name} 的模型带到下面「新增角色」里，补全岗位、名称后保存。</span>
      <Button type="primary" onClick={draft} disabled={busy}>用这些模型新增角色</Button>
    </div>
  );
}
