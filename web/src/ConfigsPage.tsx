import { Alert, Button, Drawer, Form, Input, Popconfirm, Switch, Table, Tag, Tooltip } from "antd";
import type { TableColumnsType } from "antd";
import { useEffect, useState } from "react";
import { createConfigType, deleteConfigType, fetchConfigTypes, updateConfigType, type ConfigStatus, type ConfigType, type ConfigTypeInput } from "./api";
import { useRememberedUserName } from "./identity";
import { formatDate } from "./runUi";

export const NEED_ADMIN_HINT = "只有管理员能改：先在「我是谁」里用管理员名字登录";

export function StatusTag({ status }: { status: ConfigStatus }) {
  return status === "normal" ? <Tag color="success">正常</Tag> : <Tag>停用</Tag>;
}

interface TypeForm {
  name: string;
  code: string;
  remark?: string;
  enabled: boolean;
}

/** 配置类型的新增和修改：修改时编码只读（链接和程序都靠编码找配置） */
function TypeDrawer({ editing, open, onClose, onSubmit }: { editing: ConfigType | null; open: boolean; onClose: () => void; onSubmit: (input: ConfigTypeInput) => Promise<void> }) {
  const [form] = Form.useForm<TypeForm>();
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setError(null);
    form.resetFields();
    form.setFieldsValue(editing ? { name: editing.name, code: editing.code, remark: editing.remark, enabled: editing.status === "normal" } : { enabled: true });
  }, [open, editing, form]);

  async function submit() {
    const values = await form.validateFields().catch(() => null);
    if (!values) return;
    setSaving(true);
    setError(null);
    try {
      await onSubmit({ name: values.name.trim(), code: values.code?.trim(), remark: values.remark?.trim() ?? "", status: values.enabled ? "normal" : "disabled" });
    } catch (e) {
      setError(e instanceof Error ? e.message : "保存失败");
    } finally {
      setSaving(false);
    }
  }

  return (
    <Drawer
      title={editing ? "修改配置类型" : "新增配置类型"}
      open={open}
      onClose={onClose}
      size={520}
      destroyOnHidden
      extra={<Button type="primary" loading={saving} onClick={() => void submit()}>保存</Button>}
    >
      {error && <Alert type="error" showIcon title={error} style={{ marginBottom: 16 }} />}
      <Form form={form} name="config_type" layout="vertical">
        <Form.Item name="name" label="名称" rules={[{ required: true, whitespace: true, message: "请填写名称" }, { max: 50, message: "名称不超过 50 个字" }]}>
          <Input />
        </Form.Item>
        <Form.Item
          name="code"
          label="编码"
          extra={editing ? "编码不能改" : "小写字母、数字、下划线，以字母开头，2 到 50 位"}
          rules={editing ? [] : [{ required: true, message: "请填写编码" }, { pattern: /^[a-z][a-z0-9_]{1,49}$/, message: "编码只能用小写字母、数字、下划线，以字母开头，2 到 50 位" }]}
        >
          <Input disabled={Boolean(editing)} />
        </Form.Item>
        <Form.Item name="remark" label="备注信息" rules={[{ max: 200, message: "备注不超过 200 个字" }]}>
          <Input.TextArea rows={3} />
        </Form.Item>
        <Form.Item name="enabled" label="状态" valuePropName="checked" extra="停用后这个类型下的配置不生效，程序使用代码里的默认值">
          <Switch checkedChildren="正常" unCheckedChildren="停用" />
        </Form.Item>
        <Alert type="info" showIcon title="配置里不要写密码或 API Key，写了会被拒绝。" />
      </Form>
    </Drawer>
  );
}

/** 参数配置一级列表：按名称、编码搜索；点编码进入该类型的详情页。只有管理员能新增、修改、删除。 */
export default function ConfigsPage() {
  const { name: userName } = useRememberedUserName();
  const [form] = Form.useForm<{ name?: string; code?: string }>();
  const [filter, setFilter] = useState<{ name?: string; code?: string }>({});
  const [paging, setPaging] = useState({ page: 1, pageSize: 10 });
  const [data, setData] = useState<{ rows: ConfigType[]; total: number; canWrite: boolean } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [reloadKey, setReloadKey] = useState(0);
  const [drawer, setDrawer] = useState<{ open: boolean; editing: ConfigType | null }>({ open: false, editing: null });

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    fetchConfigTypes({ ...filter, ...paging }, userName.trim(), controller.signal)
      .then((result) => { setData({ rows: result.rows, total: result.total, canWrite: result.can_write }); setError(null); })
      .catch((e: unknown) => { if (!controller.signal.aborted) setError(e instanceof Error ? e.message : "加载失败"); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [filter, paging, reloadKey, userName]);

  const canWrite = data?.canWrite ?? false;
  const reload = () => setReloadKey((k) => k + 1);

  async function remove(code: string) {
    setActionError(null);
    try {
      await deleteConfigType(code, userName.trim());
      reload();
    } catch (e) {
      setActionError(e instanceof Error ? e.message : "删除失败");
    }
  }

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
      render: (_: unknown, row) => (
        <Tooltip title={canWrite ? undefined : NEED_ADMIN_HINT}>
          <span>
            <Button type="link" size="small" disabled={!canWrite} onClick={() => setDrawer({ open: true, editing: row })}>修改</Button>
            <Popconfirm title={`删除「${row.name}」？`} description={row.item_count > 0 ? `下面还有 ${row.item_count} 条配置项，要先清空才能删。` : "删除后不能恢复。"} okText="删除" cancelText="取消" onConfirm={() => void remove(row.code)} disabled={!canWrite}>
              <Button type="link" size="small" danger disabled={!canWrite}>删除</Button>
            </Popconfirm>
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
        name="config_search"
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
        <Tooltip title={canWrite ? undefined : NEED_ADMIN_HINT}><span><Button type="primary" disabled={!canWrite} onClick={() => setDrawer({ open: true, editing: null })}>新增</Button></span></Tooltip>
        <Tooltip title="导入功能下一期开放"><span><Button disabled>导入</Button></span></Tooltip>
        <Button onClick={reload}>刷新</Button>
      </div>

      {error && <div className="config-error" role="alert">加载配置失败：{error}</div>}
      {actionError && <Alert type="error" showIcon closable onClose={() => setActionError(null)} title={actionError} />}
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

      <TypeDrawer
        editing={drawer.editing}
        open={drawer.open}
        onClose={() => setDrawer({ open: false, editing: null })}
        onSubmit={async (input) => {
          if (drawer.editing) await updateConfigType(drawer.editing.code, input, userName.trim());
          else await createConfigType(input, userName.trim());
          setDrawer({ open: false, editing: null });
          reload();
        }}
      />
    </div>
  );
}
