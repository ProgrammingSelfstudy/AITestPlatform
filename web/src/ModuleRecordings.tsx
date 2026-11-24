import { Alert, Button, Skeleton, Tag } from "antd";
import { useCallback, useEffect, useState } from "react";
import { fetchModuleRecordings, type CaseRecordings } from "./api";
import { RESULT_TEXT } from "./ModuleCases";

const formatDuration = (ms: number | null) => (ms == null ? "" : `${Math.floor(ms / 60000)}:${String(Math.floor((ms % 60000) / 1000)).padStart(2, "0")}`);

/** 录屏：这个模块里每条用例最近一次运行的录屏。地址是短期有效的签名地址，过期了点「刷新」重新取。 */
export default function ModuleRecordings({ moduleId }: { moduleId: string }) {
  const [items, setItems] = useState<CaseRecordings[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(() => {
    fetchModuleRecordings(moduleId)
      .then((list) => { setItems(list); setError(null); })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : "读取录屏失败"));
  }, [moduleId]);
  useEffect(reload, [reload]);

  return (
    <section className="mod-panel" aria-label="录屏">
      <div className="mod-panel-head">
        <h2 className="mod-panel-title">录屏</h2>
        <Button size="small" onClick={reload}>刷新（地址 10 分钟有效）</Button>
      </div>
      {error && <Alert type="error" showIcon title="读取录屏失败" description={error} />}
      {!items && !error && <Skeleton active paragraph={{ rows: 3 }} />}
      {items && items.length === 0 && <div className="mod-empty-note">还没有用例跑过，跑完才会有录屏。</div>}
      <ul className="mod-rec-list">
        {(items ?? []).map((item) => (
          <li key={item.run_id} className="mod-rec-item">
            <div className="mod-rec-head">
              <span className="mono">{item.code}</span> <strong>{item.title}</strong>{" "}
              <Tag>{item.final_result ? RESULT_TEXT[item.final_result] : "进行中"}</Tag>
              <a className="mod-link" href={`#/runs/${item.run_id}`}>查看这次运行 →</a>
            </div>
            {item.recordings.length === 0 ? (
              <div className="mod-field-meta">这次运行没有录屏（可能没录成，或设备不支持录屏）。</div>
            ) : (
              <div className="mod-rec-videos">
                {item.recordings.map((rec) => (
                  <figure key={rec.seq}>
                    <video controls preload="none" src={rec.url} aria-label={`${item.code} 录屏 ${rec.seq}`} />
                    <figcaption className="mod-field-meta">第 {rec.seq} 段{rec.duration_ms != null ? ` · ${formatDuration(rec.duration_ms)}` : ""}</figcaption>
                  </figure>
                ))}
              </div>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
