import { Button, Skeleton } from "antd";
import { useCallback, useRef, useState } from "react";
import { fetchModules, type ModuleView } from "./api";
import GenerateDialog from "./GenerateDialog";
import { formatDate, LoadError } from "./runUi";
import { REFRESH_MS, usePolling } from "./usePolling";
import "./modules.css";

const project = "hafa";

type StageTone = "pass" | "fail" | "unc" | "run" | "que" | "can";

interface ModuleStageInfo {
  text: string;
  tone: StageTone;
  pulse: boolean;
  subHint?: string;
}

/** 根据模块实时字段推导阶段文案（绝不只靠颜色区分） */
function deriveModuleStage(mod: ModuleView): ModuleStageInfo {
  if (mod.active_runs > 0) {
    return {
      text: "测试中",
      tone: "run",
      pulse: true,
      subHint: `${mod.active_runs} 条用例正在执行`,
    };
  }
  const reqStatus = mod.latest_request?.status;
  if (reqStatus === "queued" || reqStatus === "running") {
    return {
      text: "生成中",
      tone: "run",
      pulse: true,
      subHint: reqStatus === "queued" ? "生成任务排队中" : "正在生成与复核用例",
    };
  }
  if (reqStatus === "failed") {
    return {
      text: "生成失败",
      tone: "fail",
      pulse: false,
    };
  }
  if (mod.case_count > 0 && mod.approved_count < mod.case_count) {
    return {
      text: "待复核确认",
      tone: mod.needs_human_count > 0 ? "unc" : "que",
      pulse: false,
      subHint:
        mod.approved_count > 0
          ? `已有 ${mod.approved_count} 条已批准，可先发起测试`
          : undefined,
    };
  }
  if (mod.approved_count > 0) {
    return {
      text: "可以发起测试",
      tone: "pass",
      pulse: false,
    };
  }
  if (reqStatus === "cancelled") {
    return {
      text: "生成已取消",
      tone: "can",
      pulse: false,
    };
  }
  return {
    text: "暂无用例",
    tone: "que",
    pulse: false,
  };
}

export default function ModulesPage() {
  const [showGenerate, setShowGenerate] = useState(false);

  // 防闪烁轮询：数据未变化时复用旧数组引用，避免骨架屏或列表重绘闪动
  const cacheRef = useRef<{ sig: string; data: ModuleView[] } | null>(null);
  const loadModules = useCallback(async (signal: AbortSignal): Promise<ModuleView[]> => {
    const next = await fetchModules(project, signal);
    const sig = JSON.stringify(next);
    if (cacheRef.current && cacheRef.current.sig === sig) {
      return cacheRef.current.data;
    }
    cacheRef.current = { sig, data: next };
    return next;
  }, []);

  const { data: modules, error, reload } = usePolling(loadModules, REFRESH_MS);

  return (
    <div className="mod-page">
      <header className="mod-list-header">
        <div className="mod-list-title-group">
          <h1 className="mod-list-title">模块</h1>
        </div>
        <Button type="primary" onClick={() => setShowGenerate(true)}>
          新建任务（生成用例）
        </Button>
      </header>

      <div className="mod-flow-banner" role="note" aria-label="用例生成与测试流程说明">
        <span className="mod-flow-banner-label">流程说明：</span>
        <span>提交文档和截图</span>
        <span className="mod-flow-banner-arrow" aria-hidden="true">→</span>
        <span>生成用例</span>
        <span className="mod-flow-banner-arrow" aria-hidden="true">→</span>
        <span>评审员复核</span>
        <span className="mod-flow-banner-arrow" aria-hidden="true">→</span>
        <span>你确认</span>
        <span className="mod-flow-banner-arrow" aria-hidden="true">→</span>
        <span>发起测试</span>
      </div>

      {error && <LoadError title="无法加载模块列表" error={error} interval={REFRESH_MS} />}

      {!modules && !error ? (
        <section className="mod-panel" aria-label="正在加载模块">
          <div style={{ padding: 20 }}>
            <Skeleton active paragraph={{ rows: 5 }} />
          </div>
        </section>
      ) : modules && modules.length === 0 ? (
        <section className="mod-empty-card" aria-label="暂无模块">
          <h2 style={{ margin: 0, fontSize: 16, fontWeight: 600 }}>还没有任何模块</h2>
          <p style={{ margin: 0, fontSize: 13, color: "var(--text-secondary)" }}>
            点击下方按钮提交需求文档和 UI 区域截图，由模型生成并复核测试用例。
          </p>
          <Button type="primary" onClick={() => setShowGenerate(true)}>
            新建任务（生成用例）
          </Button>
        </section>
      ) : modules ? (
        <div className="mod-cards-grid">
          {modules.map((mod) => {
            const stage = deriveModuleStage(mod);
            const total = mod.case_count;
            const approvedPct = total > 0 ? Math.min(100, (mod.approved_count / total) * 100) : 0;
            const humanPct =
              total > 0
                ? Math.min(100 - approvedPct, (mod.needs_human_count / total) * 100)
                : 0;
            const otherPct = total > 0 ? Math.max(0, 100 - approvedPct - humanPct) : 0;
            const isFailed = mod.latest_request?.status === "failed";
            const moduleHref = `#/modules/${encodeURIComponent(mod.id)}`;

            return (
              <article
                key={mod.id}
                className={`mod-module-card${isFailed ? " is-failed" : mod.active_runs > 0 ? " is-testing" : ""}`}
              >
                <div className="mod-module-card-head">
                  <a className="mod-module-name" href={moduleHref}>
                    {mod.name}
                  </a>
                  <span className={`mod-badge mod-tone-${stage.tone}`}>
                    <span
                      className={`mod-badge-dot${stage.pulse ? " mod-pulse-dot" : ""}`}
                      aria-hidden="true"
                    />
                    {stage.text}
                  </span>
                </div>

                {stage.subHint && (
                  <div style={{ fontSize: 12, color: "var(--text-secondary)" }}>
                    {stage.subHint}
                  </div>
                )}

                {isFailed && (
                  <div className="mod-module-fail-box" role="alert">
                    <div className="mod-module-fail-title">生成失败原因</div>
                    <div className="mod-wrap-text">
                      {mod.latest_request?.detail || "服务端未返回详细原因"}
                    </div>
                  </div>
                )}

                <div className="mod-progress-wrap">
                  <div
                    className="mod-progress-bar"
                    role="progressbar"
                    aria-label={`${mod.name} 已批准进度`}
                    aria-valuemin={0}
                    aria-valuemax={total}
                    aria-valuenow={mod.approved_count}
                  >
                    {approvedPct > 0 && (
                      <div className="mod-progress-seg-approved" style={{ width: `${approvedPct}%` }} />
                    )}
                    {humanPct > 0 && (
                      <div className="mod-progress-seg-human" style={{ width: `${humanPct}%` }} />
                    )}
                    {otherPct > 0 && (
                      <div className="mod-progress-seg-other" style={{ width: `${otherPct}%` }} />
                    )}
                  </div>

                  <div className="mod-module-metrics">
                    <div className="mod-metric-cell">
                      <span className="mod-metric-label">用例总数</span>
                      <span className="mod-metric-val">{mod.case_count}</span>
                    </div>
                    <div className="mod-metric-cell">
                      <span className="mod-metric-label">已批准</span>
                      <span className={`mod-metric-val${mod.approved_count > 0 ? " is-pass" : ""}`}>
                        {mod.approved_count}
                      </span>
                    </div>
                    <div className="mod-metric-cell">
                      <span className="mod-metric-label">待人工确认</span>
                      <span className={`mod-metric-val${mod.needs_human_count > 0 ? " is-unc" : ""}`}>
                        {mod.needs_human_count}
                      </span>
                    </div>
                  </div>
                </div>

                <div className="mod-module-card-foot">
                  <span className="mod-mono">创建于 {formatDate(mod.created_at)}</span>
                  <a className="mod-link" href={moduleHref}>
                    查看详情 →
                  </a>
                </div>
              </article>
            );
          })}
        </div>
      ) : null}

      {showGenerate && (
        <GenerateDialog
          onClose={() => {
            setShowGenerate(false);
            reload();
          }}
        />
      )}
    </div>
  );
}
