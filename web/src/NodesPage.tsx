import { Empty, Skeleton, Space, Table, Tag, Typography } from "antd";
import type { ColumnsType } from "antd/es/table";
import { fetchNodes, type Device, type NodeView } from "./api";
import { LoadError, Pill, type PillKind } from "./runUi";
import { usePolling } from "./usePolling";

const REFRESH_MS = 3000;

// 设备状态不是用例结论：空闲用「在线」绿色以外的中性/蓝色，避免被看成「通过」
const STATUS_TAG: Record<Device["status"], { kind: PillKind; text: string }> = {
  idle: { kind: "info", text: "空闲" },
  busy: { kind: "running", text: "占用中" },
  offline: { kind: "cancelled", text: "离线" },
};

const SOURCE_NAME = { claude_cli: "Claude 命令行", codex_exec: "Codex" } as const;

const deviceColumns: ColumnsType<Device> = [
  { title: "序列号", dataIndex: "serial" },
  { title: "型号", dataIndex: "model", render: (v) => v ?? "—" },
  { title: "系统", dataIndex: "os_version", render: (v) => (v ? `Android ${v}` : "—") },
  {
    title: "分辨率",
    render: (_, d) => (d.screen_w && d.screen_h ? `${d.screen_w} × ${d.screen_h}` : "—"),
  },
  {
    title: "状态",
    dataIndex: "status",
    render: (s: Device["status"]) => <Pill kind={STATUS_TAG[s].kind}>{STATUS_TAG[s].text}</Pill>,
  },
];

function sourceTags(node: NodeView) {
  return node.caps.model_sources.length ? (
    <Space size={4} wrap>
      {node.caps.model_sources.map((m) => (
        <Tag key={m.source}>{SOURCE_NAME[m.source]} · {m.vision ? "支持视觉" : "仅文本"}</Tag>
      ))}
    </Space>
  ) : "—";
}

export default function NodesPage() {
  const { data: nodes, error } = usePolling(fetchNodes, REFRESH_MS);
  const devices = (nodes ?? []).flatMap((n) => (n.online ? n.devices : []));
  const online = (nodes ?? []).filter((n) => n.online).length;
  const idle = devices.filter((d) => d.status === "idle").length;
  const busy = devices.filter((d) => d.status === "busy").length;

  return (
    <div className="nodes-page">
      <div className="page-heading"><Typography.Title level={3}>节点与设备</Typography.Title></div>
      {error ? (
        <LoadError title="连不上平台服务端" error={error} interval={REFRESH_MS} />
      ) : !nodes ? (
        <Skeleton active />
      ) : (
        <>
          <section className="kpi-grid" aria-label="节点汇总">
            <div className="surface kpi-card"><div className="kpi-label">在线节点</div><div className="kpi-value kpi-pass">{online}</div><div className="kpi-note">共 {nodes.length} 个注册节点</div></div>
            <div className="surface kpi-card"><div className="kpi-label">空闲设备</div><div className="kpi-value kpi-run">{idle}</div><div className="kpi-note">可立即分配执行</div></div>
            <div className="surface kpi-card"><div className="kpi-label">占用中设备</div><div className="kpi-value">{busy}</div><div className="kpi-note">正在运行用例</div></div>
            <div className="surface kpi-card"><div className="kpi-label">离线节点</div><div className="kpi-value">{nodes.length - online}</div><div className="kpi-note">离线时不能接收新任务</div></div>
          </section>
          {nodes.length === 0 && <Empty description="还没有执行节点连上来" />}
          {nodes.map((n) => (
            <section key={n.node_id} className={`surface node-card${n.online ? "" : " node-offline"}`} aria-label={`节点 ${n.node_id.slice(0, 8)}`}>
              <div className="node-card-head">
                <Space size={10}>
                  <code title={n.node_id}>{n.node_id.slice(0, 8)}</code>
                  {n.online ? <Pill kind="info">在线</Pill> : <Pill kind="cancelled">离线</Pill>}
                </Space>
                <Typography.Text type="secondary" title={new Date(n.last_seen).toLocaleString("zh-CN", { hour12: false })}>
                  最近心跳 {new Date(n.last_seen).toLocaleTimeString("zh-CN", { hour12: false })}
                </Typography.Text>
              </div>
              <div className="node-card-meta">
                <span>系统 {n.caps.os}</span>
                <span>节点版本 {n.caps.node_version}</span>
                <span>模型来源 {sourceTags(n)}</span>
              </div>
              <Table<Device>
                rowKey="serial"
                columns={deviceColumns}
                dataSource={n.devices}
                pagination={false}
                size="small"
                scroll={{ x: 520 }}
                locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="这个节点没有设备" /> }}
              />
            </section>
          ))}
        </>
      )}
    </div>
  );
}
