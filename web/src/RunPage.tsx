import { Alert, Button, Descriptions, Empty, Modal, Skeleton, Space, Tag, Typography } from "antd";
import { useCallback, useEffect, useRef, useState } from "react";
import { evidenceUrl, fetchRun, type JudgeItem, type RunDetail, type RunStep } from "./api";
import RecordingPlayer, { type RecordingControls } from "./RecordingPlayer";
import { ReviewPanel, ReviewWorkspace } from "./RunReviews";
import { findRecordingPosition } from "./reviewLogic";
import { blockedLabel, FinalTag, formatDate, formatTime, isTerminal, JudgeTag, LoadError, StatusPill, VerdictTag } from "./runUi";
import { REFRESH_MS, usePolling } from "./usePolling";
import "./runpage.css";

const tools: Record<string, string> = {
  observe: "观察页面",
  tap: "点击",
  tap_xy: "按坐标点击",
  swipe: "滑动",
  drag: "拖动",
  back: "返回",
  launch_app: "启动应用",
  restart_app: "重启应用",
  wait: "等待",
  need_human: "请求人工处理",
  finish: "结束执行",
  status: "查看执行状态",
};

function shotName(reference: string): string {
  return reference.startsWith("shots/") ? reference.slice("shots/".length) : reference;
}

function stepAnchor(idx: number): string {
  return `run-step-${idx}`;
}

function stepImages(step: RunStep, files: Set<string>): string[] {
  const primary = step.file_ref ? shotName(step.file_ref) : null;
  const number = primary?.match(/^(\d+)_/)?.[1] ?? String(step.idx).padStart(2, "0");
  const names = [`${number}_observe.png`, primary ?? `${number}_after.png`];
  return [...new Set(names)].filter((name) => files.has(name));
}

function targetFor(step: RunStep): string | undefined {
  const target = step.args?.target;
  if (typeof target === "string" && target) return target;
  const point = step.args?.point;
  if (Array.isArray(point) && point.every((coordinate) => typeof coordinate === "number")) return `坐标 (${point.join(", ")})`;
  const value = step.args?.value;
  if (typeof value === "string" && value) return value;
}

const EVIDENCE_RENEW_MS = 9 * 60 * 1000;
type RunPoll = { run: RunDetail; renewInMs: number };
const runInProgress = ({ run }: RunPoll) => !isTerminal(run);

export default function RunPage({ id }: { id: string }) {
  const previous = useRef<RunDetail>(null);
  const load = useCallback(async (signal: AbortSignal): Promise<RunPoll> => {
    const run = await fetchRun(id, signal);
    const last = previous.current;
    let renewInMs = EVIDENCE_RENEW_MS;
    if (last && last.run_id === run.run_id && last.evidence_key_id === run.evidence_key_id) {
      const ageMs = (run.evidence_expires_at - last.evidence_expires_at) * 1000;
      if (ageMs >= 0 && ageMs < EVIDENCE_RENEW_MS) {
        // 保留可用的链接，避免常规轮询反复重新加载图片和录屏。
        for (const name of Object.keys(run.file_urls)) {
          run.file_urls[name] = evidenceUrl(last, name) ?? run.file_urls[name];
        }
        const recordingUrls = new Map(last.recordings.map(({ seq, url }) => [seq, url]));
        run.recordings = run.recordings.map((segment) => ({ ...segment, url: recordingUrls.get(segment.seq) ?? segment.url }));
        run.evidence_expires_at = last.evidence_expires_at;
        renewInMs -= ageMs;
      }
    }
    if (!signal.aborted) previous.current = run;
    return { run, renewInMs };
  }, [id]);
  const { data, error, reload } = usePolling(load, REFRESH_MS, runInProgress);
  useEffect(() => {
    if (data && isTerminal(data.run)) {
      const timer = window.setTimeout(reload, data.renewInMs);
      return () => window.clearTimeout(timer);
    }
  }, [data, reload]);
  const run = data?.run;
  if (error) return <LoadError title="无法加载执行详情" error={error} />;
  if (!run) return <Skeleton active />;
  return <ReviewWorkspace runId={id} finished={run.status === "finished"}><RunContent run={run} /></ReviewWorkspace>;
}

function RunContent({ run }: { run: RunDetail }) {
  const [preview, setPreview] = useState<string | null>(null);
  const [activeStep, setActiveStep] = useState<number | null>(null);
  const player = useRef<RecordingControls>(null);
  const recordings = run.recordings ?? [];
  const files = new Set(Object.keys(run.file_urls));
  const finalPageUrl = evidenceUrl(run, "final_page.txt");
  const previewUrl = preview ? evidenceUrl(run, preview) : undefined;
  const stepIds = new Set(run.run_steps.map((step) => step.idx));
  const judgeByIndex = new Map(run.judge_items.map((item) => [item.idx, item]));
  const expectedIds = new Set(run.case_spec.expected.map((item) => item.idx));
  const expected = [
    ...run.case_spec.expected,
    ...run.judge_items.filter((item) => !expectedIds.has(item.idx)).map((item) => ({ idx: item.idx, text: item.expected_text })),
  ];
  const disagreement = run.judge_status === "completed" && run.executor_result && run.final_result && run.executor_result !== run.final_result;
  const reason = run.blocked_reason ? blockedLabel(run.blocked_reason) : null;

  function thumbnail(name: string, label: string) {
    return (
      <button className="thumbnail" type="button" key={name} onClick={() => setPreview(name)} aria-label={`放大${label} ${name}`}>
        <img src={evidenceUrl(run, name)} alt={label} loading="lazy" />
        <span>{label}</span>
      </button>
    );
  }

  function jumpToStep(idx: number) {
    const target = document.getElementById(stepAnchor(idx));
    if (!target) return;
    setActiveStep(idx);
    target.focus({ preventScroll: true });
    target.scrollIntoView({ block: "center", inline: "nearest" });
  }

  function stepReference(idx: number, label: string, key: number) {
    const exists = stepIds.has(idx);
    return (
      <button className="run-step-ref" type="button" key={key} disabled={!exists}
        title={exists ? `定位到第 ${idx} 步` : `未记录第 ${idx} 步`}
        onClick={() => jumpToStep(idx)}
      >
        {label}
      </button>
    );
  }

  function evidenceText(evidence: string) {
    return evidence.split(/\bstep:([0-9]+)\b/g).map((part, index) => (
      index % 2 === 0 ? part : stepReference(Number(part), `step:${part}`, index)
    ));
  }

  function references(judge: JudgeItem) {
    const refs = judge.file_refs;
    const available = [...new Set(refs.map(shotName))].filter((name) => /\.png$/i.test(name) && files.has(name));
    const steps = new Set(refs.flatMap((reference) => {
      const match = /^step:([0-9]{1,4})$/.exec(reference);
      return match ? [Number(match[1])] : [];
    }));
    if (!available.length && !steps.size) return refs.length ? <Typography.Text type="secondary">截图未上传</Typography.Text> : null;
    return (
      <div className="thumbnails">
        {available.map((name) => thumbnail(name, name.replace(/\.png$/i, "")))}
        {[...steps].map((idx) => stepReference(idx, `步骤 ${idx} 文字`, idx))}
      </div>
    );
  }

  const selfReview = (
    <div className="self-review">
      <Space size={[8, 4]} wrap>
        <Typography.Text strong>执行员自评</Typography.Text>
        {run.executor_result ? <VerdictTag verdict={run.executor_result} /> : <Typography.Text type="secondary">尚未提交</Typography.Text>}
        <Typography.Text>仅供参考，以 Judge 为准</Typography.Text>
      </Space>
      {run.executor_result && run.executor_reason && <div className="wrap-text">{run.executor_reason}</div>}
    </div>
  );

  return (
    <div className="run-detail">
      <a className="back-link" href={`#/tasks/${run.task_id}`}>返回任务详情</a>
      <div className="page-heading">
        <Typography.Title level={3} className="wrap-text">{run.code} · {run.case_spec.title}</Typography.Title>
      </div>
      <div className="page-stack">
        <section className="surface run-summary" aria-label="执行概况">
          <div className="run-result" aria-live="polite">
            <Typography.Text strong>最终结果</Typography.Text>
            <FinalTag run={run} large />
            {!isTerminal(run) && <StatusPill status={run.status} />}
          </div>
          {reason && (
            <Alert type="warning" showIcon title={`受阻原因 · ${reason}`}
              description={run.executor_result ? undefined : run.executor_reason}
            />
          )}
          <Descriptions size="small" column={{ xs: 1, sm: 2, lg: 3 }} items={[
            { key: "model", label: "实际执行员模型", children: run.cost?.model || "尚无记录" },
            { key: "effort", label: "实际推理强度", children: run.cost?.effort || "尚无记录" },
            { key: "device", label: "设备序列号", children: run.device_serial ? <code>{run.device_serial}</code> : "尚未分配" },
            { key: "queued", label: "排队时间", children: formatDate(run.queued_at) },
            { key: "started", label: "开始时间", children: run.started_at ? formatDate(run.started_at) : "—" },
            { key: "finished", label: "结束时间", children: run.finished_at ? formatDate(run.finished_at) : "—" },
          ]} />
          {disagreement
            ? <Alert type="warning" showIcon title="执行员自评与 Judge 结论不一致" description={selfReview} />
            : selfReview}
        </section>

        <RecordingPlayer ref={player} recordings={recordings} />

        <section className="surface run-panel" aria-label="整条用例审核">
          <div className="panel-heading"><Typography.Title level={4}>整条用例审核</Typography.Title></div>
          <ReviewPanel itemIdx={null} label="整条用例" />
        </section>

        <div className="run-grid">
          <section className="surface run-panel" aria-label="执行步骤">
            <div className="panel-heading run-detail-heading">
              <Typography.Title level={4}>执行步骤时间线</Typography.Title>
              <Typography.Text type="secondary" aria-live="polite">已记录 {run.run_steps.length} 步</Typography.Text>
            </div>
            {run.run_steps.length ? (
              <ol className="run-timeline" role="list">
                {run.run_steps.map((step) => {
                  const action = tools[step.tool] ?? step.tool;
                  const target = targetFor(step);
                  const position = findRecordingPosition(recordings, step.at_ms);
                  const seekReason = !recordings.length ? "本次执行没有录屏"
                    : step.at_ms == null ? "此步骤没有录屏时间（旧记录）"
                      : !position ? "此步骤不在可定位的录屏范围内" : null;
                  const labels = Array.isArray(step.result?.new_labels)
                    ? step.result.new_labels.filter((label) => typeof label === "string")
                    : [];
                  return (
                    <li className="run-timeline-step step-content" key={step.idx} id={stepAnchor(step.idx)} tabIndex={-1}
                      aria-label={`第 ${step.idx} 步 · ${action}`} aria-current={activeStep === step.idx ? "step" : undefined}
                    >
                      <span className="run-step-marker" aria-hidden="true">{String(step.idx).padStart(2, "0")}</span>
                      <div className="run-step-heading">
                        <div className="run-step-action-line">
                          <Tag className="run-step-action">{action}</Tag>
                          {target && <Typography.Text strong className="wrap-text">{target}</Typography.Text>}
                        </div>
                        <Typography.Text type="secondary" className="step-time">{formatTime(step.ts)}</Typography.Text>
                      </div>
                      {labels.length > 0 && (
                        <div className="step-text">
                          <Typography.Text type="secondary">新出现的文字</Typography.Text>
                          <ul>{labels.map((label, index) => <li className="wrap-text" key={index}>{label}</li>)}</ul>
                        </div>
                      )}
                      <div className="thumbnails">
                        {stepImages(step, files).map((name) => thumbnail(name, name.endsWith("_observe.png") ? "操作前观察" : "操作后截图"))}
                      </div>
                      <div className="step-seek">
                        <Button type="link" size="small" disabled={!position} onClick={() => position && player.current?.seek(position)}>
                          跳到录屏
                        </Button>
                        {seekReason && <Typography.Text type="secondary">{seekReason}</Typography.Text>}
                      </div>
                    </li>
                  );
                })}
              </ol>
            ) : (
              <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={isTerminal(run) ? "本次执行没有记录步骤" : "等待节点上报步骤"} />
            )}
            {finalPageUrl && (
              <a href={finalPageUrl} target="_blank" rel="noreferrer">打开终态页面文字</a>
            )}
          </section>

          <section className="surface run-panel" aria-label="逐条预期">
            <div className="panel-heading run-detail-heading">
              <Typography.Title level={4}>逐条预期</Typography.Title>
              {run.judge_status === "completed" && <JudgeTag status={run.judge_status} />}
            </div>
            {run.judge_status !== "completed" && (
              <Alert type="warning" showIcon title="Judge 未完成"
                description={run.judge_status === "failed" ? "判定执行失败。" :
                  isTerminal(run) ? "本次执行没有完成逐条判定。" : "等待 Judge 完成逐条判定。"}
              />
            )}
            <details className="case-steps">
              <summary>查看用例步骤</summary>
              <ol>{run.case_spec.steps.map((step, index) => <li className="wrap-text" key={index}>{step}</li>)}</ol>
            </details>
            {expected.map((item) => {
              const judge = judgeByIndex.get(item.idx);
              return (
                <article className="judge-item" data-verdict={judge?.verdict} key={item.idx}>
                  <div className="panel-heading">
                    <Typography.Text strong className="wrap-text">{item.idx}. {item.text || "预期原文缺失"}</Typography.Text>
                    {judge ? <VerdictTag verdict={judge.verdict} /> : <Tag>未判定</Tag>}
                  </div>
                  {judge && (
                    <>
                      <div className="run-evidence wrap-text">证据：{evidenceText(judge.evidence || "—")}</div>
                      {references(judge)}
                      <ReviewPanel itemIdx={item.idx} label={`第 ${item.idx} 条预期`} />
                    </>
                  )}
                </article>
              );
            })}
            {!expected.length && <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="没有逐条预期" />}
          </section>
        </div>
      </div>
      {preview && previewUrl && (
        <Modal open title="截图预览" footer={null} onCancel={() => setPreview(null)} width={680} centered>
          <img className="preview" src={previewUrl} alt={preview} />
        </Modal>
      )}
    </div>
  );
}
