import { Alert, Checkbox, Input, Select, Switch } from "antd";
import { useEffect, useMemo, useState } from "react";
import { ROLE_SLOT_LIMIT, ROLE_SOURCES, SLOT_LABEL, SLOT_ORDER, fetchNodeRoles, fetchNodes, fetchRoleLimits, parseFlowRoles, type NodeView, type RoleView } from "./api";
import { useRememberedUserName } from "./identity";

type Selected = Record<string, string[]>;

const sourceLabel = (source: string) => ROLE_SOURCES.find((item) => item.value === source)?.label ?? source;

/** 按岗位顺序、岗位内保持勾选顺序，写成存进配置里的 JSON：{"岗位": ["角色名", ...]} */
function toValue(selected: Selected): string {
  const ordered: Selected = {};
  for (const slot of SLOT_ORDER) if (selected[slot]?.length) ordered[slot] = selected[slot];
  return JSON.stringify(ordered);
}

/**
 * 流程模板的「值」编辑：从所选节点上已有的角色里勾选，不用手写 JSON。
 * - 只列出这个节点上已经配好的角色（带岗位、来源、模型、检测状态）；
 * - 每个岗位最多勾几个，由参数配置里的「岗位角色上限」决定（读不到用默认值）；
 * - 值里已有、但这个节点上没有的角色单独列出，可以取消，不会悄悄丢掉；
 * - 读不到角色时给原因，并能切到手写 JSON，不把人堵死。
 */
export default function FlowTemplateEditor({ value, onChange }: { value?: string; onChange?: (value: string) => void }) {
  const { name: userName } = useRememberedUserName();
  const [nodes, setNodes] = useState<NodeView[]>([]);
  const [nodeId, setNodeId] = useState("");
  const [roles, setRoles] = useState<RoleView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [limits, setLimits] = useState<Record<string, number>>({});
  const [raw, setRaw] = useState(false);

  const selected: Selected = useMemo(() => parseFlowRoles(value ?? "") ?? {}, [value]);

  useEffect(() => {
    const controller = new AbortController();
    void fetchRoleLimits(controller.signal).then((l) => { if (!controller.signal.aborted) setLimits(l); });
    fetchNodes(controller.signal)
      .then((list) => {
        if (controller.signal.aborted) return;
        const online = list.filter((n) => n.online);
        setNodes(online);
        setNodeId((prev) => prev || online[0]?.node_id || "");
        if (online.length === 0) setError("现在没有在线节点，读不到角色");
      })
      .catch((e: unknown) => { if (!controller.signal.aborted) setError(e instanceof Error ? e.message : "读取节点失败"); });
    return () => controller.abort();
  }, []);

  useEffect(() => {
    if (!nodeId) return;
    const controller = new AbortController();
    setRoles(null);
    setError(null);
    fetchNodeRoles(nodeId, userName.trim(), controller.signal)
      .then((result) => {
        if (controller.signal.aborted) return;
        if (!result.online) setError("这个节点离线，读不到角色");
        setRoles(result.online ? result.roles : []);
      })
      .catch((e: unknown) => { if (!controller.signal.aborted) setError(e instanceof Error ? e.message : "读取角色失败"); });
    return () => controller.abort();
  }, [nodeId, userName]);

  const limitOf = (slot: string) => limits[slot] ?? ROLE_SLOT_LIMIT;
  const nodeRoleNames = useMemo(() => new Set((roles ?? []).map((r) => r.name)), [roles]);
  const missing = useMemo(
    () => (roles ? SLOT_ORDER.flatMap((slot) => (selected[slot] ?? []).filter((name) => !(roles ?? []).some((r) => r.slot === slot && r.name === name)).map((name) => ({ slot, name }))) : []),
    [roles, selected],
  );

  /** 选数量 N：保留已勾的（按原顺序）在前，不够就从这个节点该岗位还没勾的角色里按顺序补，多了从后面去掉。节点上没有的旧角色不动。 */
  function setCount(slot: string, group: RoleView[], n: number) {
    const current = selected[slot] ?? [];
    const onNode = new Set(group.map((r) => r.name));
    const kept = current.filter((name) => onNode.has(name)).slice(0, n);
    const fill = group.map((r) => r.name).filter((name) => !kept.includes(name)).slice(0, Math.max(0, n - kept.length));
    const next = [...kept, ...fill, ...current.filter((name) => !onNode.has(name))];
    const updated = { ...selected, [slot]: next };
    if (next.length === 0) delete updated[slot];
    onChange?.(toValue(updated));
  }

  function toggle(slot: string, name: string, on: boolean) {
    const current = selected[slot] ?? [];
    const next = on ? [...current.filter((n) => n !== name), name] : current.filter((n) => n !== name);
    const updated = { ...selected, [slot]: next };
    if (next.length === 0) delete updated[slot];
    onChange?.(toValue(updated));
  }

  if (raw) {
    return (
      <div className="flow-editor">
        <Input.TextArea rows={4} value={value} onChange={(e) => onChange?.(e.target.value)} aria-label="流程 JSON" className="mono" />
        <label className="flow-editor-switch"><Switch size="small" checked={raw} onChange={setRaw} /> 手写 JSON</label>
      </div>
    );
  }

  return (
    <div className="flow-editor">
      <div className="flow-editor-node">
        <span>从哪个节点上选角色</span>
        <Select
          aria-label="节点"
          value={nodeId || undefined}
          placeholder="选择在线节点"
          style={{ minWidth: 260 }}
          options={nodes.map((n) => ({ value: n.node_id, label: `${n.node_id.slice(0, 8)}… · ${n.caps.os}` }))}
          onChange={setNodeId}
        />
      </div>
      {error && <Alert type="warning" showIcon title={error} description="可以先打开下面的「手写 JSON」直接填。" />}
      {roles && roles.length === 0 && !error && <Alert type="info" showIcon title="这个节点上还没有角色，先去「角色与模型」配置。" />}
      {SLOT_ORDER.map((slot) => {
        const group = (roles ?? []).filter((r) => r.slot === slot);
        if (group.length === 0) return null;
        const picked = selected[slot] ?? [];
        const full = picked.length >= limitOf(slot);
        return (
          <div key={slot} className="flow-editor-group">
            <div className="flow-editor-group-head">
              <strong>{SLOT_LABEL[slot]}</strong>
              <span className="flow-editor-count">
                数量
                <Select
                  size="small"
                  aria-label={`${SLOT_LABEL[slot]}数量`}
                  value={picked.filter((n) => nodeRoleNames.has(n)).length}
                  style={{ width: 64, marginLeft: 8 }}
                  options={Array.from({ length: Math.min(limitOf(slot), group.length) + 1 }, (_, n) => ({ value: n, label: String(n) }))}
                  onChange={(n) => setCount(slot, group, n)}
                />
                <span> / 上限 {limitOf(slot)}</span>
              </span>
            </div>
            {group.map((role) => {
              const checked = picked.includes(role.name);
              return (
                <Checkbox key={role.id} checked={checked} disabled={!checked && full} onChange={(e) => toggle(slot, role.name, e.target.checked)}>
                  {role.name}
                  <span className="flow-editor-meta"> · {sourceLabel(role.source)} · {role.default.model || "未设模型"}{role.check.status === "ok" ? "" : " · 未检测"}</span>
                </Checkbox>
              );
            })}
            {full && <div className="flow-editor-hint">已选满 {limitOf(slot)} 个（上限在「岗位角色上限」里设置，调大后可以多选）</div>}
          </div>
        );
      })}
      {missing.length > 0 && (
        <div className="flow-editor-group">
          <div className="flow-editor-group-head"><strong>流程里有、但这个节点上没有的角色</strong></div>
          {missing.map(({ slot, name }) => (
            <Checkbox key={`${slot}:${name}`} checked onChange={() => toggle(slot, name, false)}>
              {name}<span className="flow-editor-meta"> · {SLOT_LABEL[slot]}（取消勾选就从流程里去掉）</span>
            </Checkbox>
          ))}
        </div>
      )}
      <div className="flow-editor-hint">选 N 个 = 这 N 个不同的角色同时干同一件事：拿到的输入一样、各干各的，每个人的结果单独记录，结果可能不一样；不是同一个角色跑 N 遍。</div>
      <label className="flow-editor-switch"><Switch size="small" checked={raw} onChange={setRaw} /> 手写 JSON</label>
    </div>
  );
}
