import { Alert, Button, Drawer, Form, Input, InputNumber, Popconfirm, Switch, Table, Tooltip } from "antd";
import type { TableColumnsType } from "antd";
import { useEffect, useState } from "react";
import FlowTemplateEditor from "./FlowTemplateEditor";
import { NEED_ADMIN_HINT, StatusTag } from "./ConfigsPage";
import { createConfigItem, deleteConfigItem, describeFlowRoles, fetchConfigDetail, parseFlowRoles, updateConfigItem, type ConfigItem, type ConfigItemInput, type ConfigTypeDetail } from "./api";
import { useRememberedUserName } from "./identity";

interface ItemForm {
  name: string;
  code: string;
  value: string;
  label?: string;
  group_name?: string;
  sort: number;
  remark?: string;
  enabled: boolean;
}

/** 配置项的新增和修改：修改时编码只读 */
function ItemDrawer({ typeCode, editing, open, onClose, onSubmit }: { typeCode: string; editing: ConfigItem | null; open: boolean; onClose: () => void; onSubmit: (input: ConfigItemInput) => Promise<void> }) {
  const [form] = Form.useForm<ItemForm>();
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setError(null);
    form.resetFields();
    form.setFieldsValue(
      editing
        ? { name: editing.name, code: editing.code, value: editing.value, label: editing.label, group_name: editing.group_name, sort: editing.sort, remark: editing.remark, enabled: editing.status === "normal" }
        : { sort: 0, enabled: true },
    );
  }, [open, editing, form]);

  async function submit() {
    const values = await form.validateFields().catch(() => null);
    if (!values) return;
    setSaving(true);
    setError(null);
    try {
      await onSubmit({
        name: values.name.trim(),
        code: values.code?.trim(),
        value: values.value.trim(),
        label: values.label?.trim() ?? "",
        group_name: values.group_name?.trim() ?? "",
        sort: values.sort ?? 0,
        remark: values.remark?.trim() ?? "",
        status: values.enabled ? "normal" : "disabled",
      });
    } catch (e) {
      setError(e instanceof Error ? e.message : "保存失败");
    } finally {
      setSaving(false);
    }
  }

  return (
    <Drawer
      title={editing ? "修改配置项" : "新增配置项"}
      open={open}
      onClose={onClose}
      size={520}
      destroyOnHidden
      extra={<Button type="primary" loading={saving} onClick={() => void submit()}>保存</Button>}
    >
      {error && <Alert type="error" showIcon title={error} style={{ marginBottom: 16 }} />}
      <Form form={form} name="config_item" layout="vertical">
        <Form.Item name="name" label="名称" rules={[{ required: true, whitespace: true, message: "请填写名称" }, { max: 50, message: "名称不超过 50 个字" }]}>
          <Input />
        </Form.Item>
        <Form.Item
          name="code"
          label="编码"
          extra={editing ? "编码不能改" : "同一类型下不能重复；小写字母、数字、下划线，以字母开头"}
          rules={editing ? [] : [{ required: true, message: "请填写编码" }, { pattern: /^[a-z][a-z0-9_]{1,49}$/, message: "编码只能用小写字母、数字、下划线，以字母开头，2 到 50 位" }]}
        >
          <Input disabled={Boolean(editing)} />
        </Form.Item>
        {typeCode === "flow_template" ? (
          <Form.Item
            name="value"
            label="流程里的角色"
            extra="从节点上已有的角色里勾选，每个岗位最多勾几个由「岗位角色上限」决定；新建任务选这个流程时，按角色名在所选节点上自动选好。"
            rules={[{ required: true, message: "请至少勾选一个角色" }, { validator: (_rule, v: string) => (parseFlowRoles(v ?? "") ? Promise.resolve() : Promise.reject(new Error("请至少勾选一个角色（或检查手写的 JSON 格式）"))) }]}
          >
            <FlowTemplateEditor />
          </Form.Item>
        ) : (
          <Form.Item name="value" label="值" rules={[{ required: true, whitespace: true, message: "请填写值" }, { max: 2000, message: "值不超过 2000 个字" }]}>
            <Input.TextArea rows={3} />
          </Form.Item>
        )}
        <Form.Item name="label" label="标签" rules={[{ max: 32, message: "标签不超过 32 个字" }]}><Input /></Form.Item>
        <Form.Item name="group_name" label="分组" rules={[{ max: 32, message: "分组不超过 32 个字" }]}><Input /></Form.Item>
        <Form.Item name="sort" label="排序" extra="数字小的排前面"><InputNumber precision={0} style={{ width: "100%" }} /></Form.Item>
        <Form.Item name="remark" label="备注信息" rules={[{ max: 200, message: "备注不超过 200 个字" }]}><Input.TextArea rows={2} /></Form.Item>
        <Form.Item name="enabled" label="状态" valuePropName="checked" extra="停用后这一项不生效，程序使用代码里的默认值">
          <Switch checkedChildren="正常" unCheckedChildren="停用" />
        </Form.Item>
        <Alert type="info" showIcon title="配置里不要写密码或 API Key，写了会被拒绝。" />
      </Form>
    </Drawer>
  );
}

/** 参数配置二级详情：一个配置类型下的所有配置项。只有管理员能新增、修改、禁用、删除。 */
export default function ConfigDetailPage({ code }: { code: string }) {
  const { name: userName } = useRememberedUserName();
  const user = userName.trim();
  const [paging, setPaging] = useState({ page: 1, pageSize: 10 });
  const [detail, setDetail] = useState<ConfigTypeDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [reloadKey, setReloadKey] = useState(0);
  const [selected, setSelected] = useState<ConfigItem[]>([]);
  const [drawer, setDrawer] = useState<{ open: boolean; editing: ConfigItem | null }>({ open: false, editing: null });

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    fetchConfigDetail(code, paging.page, paging.pageSize, user, controller.signal)
      .then((result) => { setDetail(result); setError(null); })
      .catch((e: unknown) => { if (!controller.signal.aborted) setError(e instanceof Error ? e.message : "加载失败"); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [code, paging, reloadKey, user]);

  const canWrite = detail?.can_write ?? false;
  const reload = () => { setSelected([]); setReloadKey((k) => k + 1); };
  const hint = canWrite ? undefined : NEED_ADMIN_HINT;

  const toInput = (row: ConfigItem, status: ConfigItem["status"]): ConfigItemInput => ({
    name: row.name, value: row.value, label: row.label, group_name: row.group_name, sort: row.sort, remark: row.remark, status,
  });

  async function act(work: () => Promise<void>, failure: string) {
    setActionError(null);
    try {
      await work();
      reload();
    } catch (e) {
      setActionError(e instanceof Error ? e.message : failure);
    }
  }

  const columns: TableColumnsType<ConfigItem> = [
    { title: "编号", dataIndex: "id", width: 80 },
    { title: "名称", dataIndex: "name" },
    {
      title: "值",
      dataIndex: "value",
      ellipsis: { showTitle: true },
      render: (value: string) => {
        // 流程模板显示成「执行员：A；判定员：B、C」，不显示一串 JSON；解析不了才显示原文
        const flow = code === "flow_template" ? parseFlowRoles(value) : null;
        return flow ? <span title={value}>{describeFlowRoles(flow)}</span> : <span className="mono">{value}</span>;
      },
    },
    { title: "标签", dataIndex: "label", width: 80 },
    { title: "编码", dataIndex: "code", render: (value: string) => <span className="mono">{value}</span> },
    { title: "分组", dataIndex: "group_name", width: 100 },
    { title: "排序", dataIndex: "sort", width: 70 },
    { title: "备注信息", dataIndex: "remark", ellipsis: true },
    { title: "状态", dataIndex: "status", width: 90, render: (status: ConfigItem["status"]) => <StatusTag status={status} /> },
    {
      title: "操作",
      width: 220,
      render: (_: unknown, row) => (
        <Tooltip title={hint}>
          <span>
            <Button type="link" size="small" disabled={!canWrite} onClick={() => setDrawer({ open: true, editing: row })}>修改</Button>
            <Button type="link" size="small" disabled={!canWrite} onClick={() => void act(() => updateConfigItem(code, row.id, toInput(row, row.status === "normal" ? "disabled" : "normal"), user), "切换状态失败")}>
              {row.status === "normal" ? "禁用" : "启用"}
            </Button>
            <Popconfirm title={`删除「${row.name}」？`} description="删除后不能恢复。" okText="删除" cancelText="取消" disabled={!canWrite} onConfirm={() => void act(() => deleteConfigItem(code, row.id, user), "删除失败")}>
              <Button type="link" size="small" danger disabled={!canWrite}>删除</Button>
            </Popconfirm>
          </span>
        </Tooltip>
      ),
    },
  ];

  const type = detail?.type;

  return (
    <div className="config-page">
      <div className="page-heading">
        <div className="page-title-block">
          <nav className="config-crumb" aria-label="配置层级">
            <a href="#/configs">参数配置</a>
            <span className="breadcrumb-sep" aria-hidden="true">/</span>
            <strong>{type ? `${type.name}（${type.code}）` : code}</strong>
          </nav>
          {type?.remark && <p className="page-desc">{type.remark}</p>}
        </div>
      </div>

      {type?.status === "disabled" && <Alert type="warning" showIcon title="这个配置类型已停用，下面的配置项不生效，程序使用代码里的默认值。" />}
      {error && <Alert type="error" showIcon title="加载失败" description={error} />}
      {actionError && <Alert type="error" showIcon closable onClose={() => setActionError(null)} title={actionError} />}

      <div className="config-toolbar">
        <Tooltip title={hint}><span><Button type="primary" disabled={!canWrite} onClick={() => setDrawer({ open: true, editing: null })}>新增</Button></span></Tooltip>
        <Tooltip title={hint}><span><Button disabled={!canWrite || selected.length !== 1} onClick={() => setDrawer({ open: true, editing: selected[0] })}>修改</Button></span></Tooltip>
        <Popconfirm
          title={`删除选中的 ${selected.length} 条配置项？`}
          description="删除后不能恢复。"
          okText="删除"
          cancelText="取消"
          disabled={!canWrite || selected.length === 0}
          onConfirm={() => void act(async () => { for (const row of selected) await deleteConfigItem(code, row.id, user); }, "删除失败")}
        >
          <span><Button danger disabled={!canWrite || selected.length === 0}>删除</Button></span>
        </Popconfirm>
      </div>

      <Table<ConfigItem>
        rowKey="id"
        columns={columns}
        dataSource={detail?.items.rows ?? []}
        loading={loading}
        rowSelection={{ selectedRowKeys: selected.map((row) => row.id), onChange: (_keys, rows) => setSelected(rows) }}
        locale={{ emptyText: "这个类型下还没有配置项" }}
        pagination={{
          current: paging.page,
          pageSize: paging.pageSize,
          total: detail?.items.total ?? 0,
          showSizeChanger: true,
          showQuickJumper: true,
          showTotal: (total) => `共 ${total} 条`,
          onChange: (page, pageSize) => setPaging({ page, pageSize }),
        }}
      />

      <ItemDrawer
        typeCode={code}
        editing={drawer.editing}
        open={drawer.open}
        onClose={() => setDrawer({ open: false, editing: null })}
        onSubmit={async (input) => {
          if (drawer.editing) await updateConfigItem(code, drawer.editing.id, input, user);
          else await createConfigItem(code, input, user);
          setDrawer({ open: false, editing: null });
          reload();
        }}
      />
    </div>
  );
}
