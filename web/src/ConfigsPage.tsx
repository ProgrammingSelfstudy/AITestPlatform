import { Button, Form, Input, Table, Tag, Tooltip } from "antd";
import type { TableColumnsType } from "antd";
import { useEffect, useState } from "react";
import { fetchConfigTypes, type ConfigStatus, type ConfigType } from "./api";
import { formatDate } from "./runUi";

const WRITE_HINT = "写入功能下一期开放，只有管理员能改";

export function StatusTag({ status }: { status: ConfigStatus }) {
  return status === "normal" ? <Tag color="success">正常</Tag> : <Tag>停用</Tag>;
}

/** 参数配置一级列表：按名称、编码搜索；点编码进入该类型的详情页。P1 只读。 */
export default function ConfigsPage() {
  const [form] = Form.useForm<{ name?: string; code?: string }>();
  const [filter, setFilter] = useState<{ name?: string; code?: string }>({});
  const [paging, setPaging] = useState({ page: 1, pageSize: 10 });
  const [data, setData] = useState<{ rows: ConfigType[]; total: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    fetchConfigTypes({ ...filter, ...paging }, controller.signal)
      .then((result) => { setData({ rows: result.rows, total: result.total }); setError(null); })
      .catch((e: unknown) => { if (!controller.signal.aborted) setError(e instanceof Error ? e.message : "加载失败"); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [filter, paging, reloadKey]);

  const columns: TableColumnsType<ConfigType> = [
    { title: "ID", dataIndex: "id", width: 80 },
    { title: "名称", dataIndex: "name" },
    {
      title: "编码",
      dataIndex: "code",
      render: (code: string) => <a className="mono" href={`#/configs/${encodeURIComponent(code)}`}>{code}</a>,
    },
    { title: "配置项数", dataIndex: "item_count", width: 100 },
    { title: "备注信息", dataIndex: "remark", ellipsis: true },
    { title: "状态", dataIndex: "status", width: 90, render: (status: ConfigStatus) => <StatusTag status={status} /> },
    { title: "更新时间", dataIndex: "updated_at", width: 180, render: (value: string) => formatDate(value) },
    {
      title: "操作",
      width: 150,
      render: () => (
        <Tooltip title={WRITE_HINT}>
          <span>
            <Button type="link" size="small" disabled>修改</Button>
            <Button type="link" size="small" disabled danger>删除</Button>
          </span>
        </Tooltip>
      ),
    },
  ];

  return (
    <div className="config-page">
      <div className="page-heading">
        <div className="page-title-block">
          <h3 className="ant-typography">参数配置</h3>
          <p className="page-desc">平台的所有配置集中在这里；点编码进入详情。所有人可看，只有管理员能改。</p>
        </div>
      </div>

      <Form
        form={form}
        layout="inline"
        className="config-search"
        onFinish={(values) => { setPaging((p) => ({ ...p, page: 1 })); setFilter(values); }}
      >
        <Form.Item name="name" label="名称"><Input placeholder="请输入名称" allowClear /></Form.Item>
        <Form.Item name="code" label="类型编码"><Input placeholder="请输入类型编码" allowClear /></Form.Item>
        <Form.Item>
          <Button type="primary" htmlType="submit">搜索</Button>
        </Form.Item>
        <Form.Item>
          <Button onClick={() => { form.resetFields(); setPaging((p) => ({ ...p, page: 1 })); setFilter({}); }}>重置</Button>
        </Form.Item>
      </Form>

      <div className="config-toolbar">
        <Tooltip title={WRITE_HINT}><span><Button type="primary" disabled>新增</Button></span></Tooltip>
        <Tooltip title={WRITE_HINT}><span><Button disabled>导入</Button></span></Tooltip>
        <Button onClick={() => setReloadKey((k) => k + 1)}>刷新</Button>
      </div>

      {error && <div className="config-error" role="alert">加载配置失败：{error}</div>}
      <Table<ConfigType>
        rowKey="id"
        columns={columns}
        dataSource={data?.rows ?? []}
        loading={loading}
        locale={{ emptyText: filter.name || filter.code ? "没有符合条件的配置，试试点「重置」" : "还没有配置" }}
        pagination={{
          current: paging.page,
          pageSize: paging.pageSize,
          total: data?.total ?? 0,
          showSizeChanger: true,
          showQuickJumper: true,
          showTotal: (total) => `共 ${total} 条`,
          onChange: (page, pageSize) => setPaging({ page, pageSize }),
        }}
      />
    </div>
  );
}
