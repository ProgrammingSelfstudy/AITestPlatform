import { Alert, Button, Popconfirm, Skeleton } from "antd";
import { useCallback, useEffect, useRef, useState } from "react";
import { addModuleShots, deleteModuleShot, fetchModuleShots, moduleShotUrl, type ModuleShot } from "./api";
import { useGuardedWrite } from "./useGuardedWrite";
import { formatDate } from "./runUi";

const MAX_SHOTS = 100;
const MAX_IMAGE_BYTES = 3 * 1024 * 1024;
const MAX_TOTAL_BYTES = 100 * 1024 * 1024;

const formatSize = (bytes: number) => (bytes === 0 ? "0 KB" : bytes < 1024 * 1024 ? `${Math.max(1, Math.round(bytes / 1024))} KB` : `${(bytes / 1024 / 1024).toFixed(1)} MB`);

function readAsBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result).replace(/^data:[^;]+;base64,/, ""));
    reader.onerror = () => reject(new Error(`「${file.name}」读取失败`));
    reader.readAsDataURL(file);
  });
}

/** 模块的 UI 图：能上传、能增加、能删除。图片只收 PNG / JPEG，单张 ≤ 3MB，模块里最多 100 张、合计 ≤ 100MB。 */
export default function ModuleShots({ moduleId, userName }: { moduleId: string; userName: string }) {
  const [shots, setShots] = useState<ModuleShot[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  const write = useGuardedWrite(userName.trim());

  const reload = useCallback(() => {
    fetchModuleShots(moduleId)
      .then((list) => { setShots(list); setError(null); })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : "读取 UI 图失败"));
  }, [moduleId]);
  useEffect(reload, [reload]);

  const total = (shots ?? []).reduce((sum, s) => sum + s.size, 0);

  async function upload(files: File[]) {
    setNotice(null);
    write.setError(null);
    const problems: string[] = [];
    const ok: File[] = [];
    for (const file of files) {
      if (!["image/png", "image/jpeg"].includes(file.type)) problems.push(`「${file.name}」不是 PNG / JPEG 图片`);
      else if (file.size > MAX_IMAGE_BYTES) problems.push(`「${file.name}」超过 3MB，请裁剪后再传`);
      else ok.push(file);
    }
    if ((shots?.length ?? 0) + ok.length > MAX_SHOTS) problems.push(`这个模块最多 ${MAX_SHOTS} 张 UI 图，现在已有 ${shots?.length ?? 0} 张`);
    else if (total + ok.reduce((s, f) => s + f.size, 0) > MAX_TOTAL_BYTES) problems.push("UI 图合计不能超过 100MB");
    if (problems.length > 0) {
      setNotice(problems.join("；"));
      if (problems.some((p) => p.includes("最多") || p.includes("合计"))) return;
    }
    if (ok.length === 0) return;
    await write.run(async (token) => {
      const encoded = await Promise.all(ok.map(async (f) => ({ name: f.name, data_b64: await readAsBase64(f) })));
      const result = await addModuleShots(moduleId, encoded, token, userName.trim());
      setNotice(`新增 ${result.added} 张${result.duplicates > 0 ? `，${result.duplicates} 张和已有的一样，没有重复保存` : ""}${problems.length > 0 ? `；${problems.join("；")}` : ""}`);
      reload();
    });
  }

  async function remove(shot: ModuleShot) {
    setNotice(null);
    await write.run(async (token) => {
      await deleteModuleShot(moduleId, shot.id, token, userName.trim());
      setNotice(`已删除「${shot.name}」`);
      reload();
    });
  }

  return (
    <section className="mod-panel mod-shots" aria-label="UI 图">
      <div className="mod-panel-head">
        <h2 className="mod-panel-title">UI 图</h2>
        <span className="mod-field-meta">{shots ? `${shots.length} / ${MAX_SHOTS} 张 · 合计 ${formatSize(total)} / 100 MB` : ""}</span>
      </div>
      <div
        className={`mod-shots-drop${dragOver ? " is-over" : ""}`}
        onDragOver={(e) => { e.preventDefault(); setDragOver(true); }}
        onDragLeave={() => setDragOver(false)}
        onDrop={(e) => { e.preventDefault(); setDragOver(false); void upload(Array.from(e.dataTransfer.files)); }}
      >
        <Button type="primary" disabled={write.busy} loading={write.busy} onClick={() => input.current?.click()}>上传 UI 图</Button>
        <span className="mod-field-meta">也可以把图片拖到这里；PNG / JPEG，单张不超过 3MB，同一张图不会重复保存</span>
        <input
          ref={input}
          type="file"
          accept="image/png,image/jpeg"
          multiple
          hidden
          aria-label="选择 UI 图"
          onChange={(e) => { const files = Array.from(e.target.files ?? []); e.target.value = ""; void upload(files); }}
        />
      </div>
      {write.error && <Alert type="error" showIcon closable onClose={() => write.setError(null)} title={write.error} />}
      {notice && <Alert type="info" showIcon closable onClose={() => setNotice(null)} title={notice} />}
      {error && <Alert type="error" showIcon title="读取 UI 图失败" description={error} />}
      {!shots && !error && <Skeleton active paragraph={{ rows: 3 }} />}
      {shots && shots.length === 0 && <div className="mod-empty-note">这个模块还没有 UI 图，点「上传 UI 图」添加。</div>}
      <ul className="mod-shot-grid">
        {(shots ?? []).map((shot) => (
          <li key={shot.id} className="mod-shot-card">
            <a href={moduleShotUrl(moduleId, shot.id)} target="_blank" rel="noreferrer" aria-label={`查看大图：${shot.name}`}>
              <img src={moduleShotUrl(moduleId, shot.id)} alt={shot.name} loading="lazy" />
            </a>
            <div className="mod-shot-name" title={shot.name}>{shot.name}</div>
            <div className="mod-field-meta">{formatSize(shot.size)} · {shot.created_by ?? "未知"} · {formatDate(shot.created_at)}</div>
            <Popconfirm title={`删除「${shot.name}」？`} description="删除后不能恢复。" okText="删除" cancelText="取消" onConfirm={() => void remove(shot)}>
              <Button size="small" danger disabled={write.busy}>删除</Button>
            </Popconfirm>
          </li>
        ))}
      </ul>
      {write.dialog}
    </section>
  );
}
