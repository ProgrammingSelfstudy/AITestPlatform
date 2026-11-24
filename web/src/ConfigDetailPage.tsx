import { Alert, Button, Table, Tooltip } from "antd";
import type { TableColumnsType } from "antd";
import { useEffect, useState } from "react";
import { StatusTag } from "./ConfigsPage";
import { fetchConfigDetail, type ConfigItem, type ConfigTypeDetail } from "./api";

const WRITE_HINT = "写入功能下一期开放，只有管理员能改";

/** 参数配置二级详情：一个配置类型下的所有配置项。P1 只读。 */
export default function ConfigDetailPage({ code }: { code: string }) {
  const [paging, setPaging] = useState({ page: 1, pageSize: 10 });
  const [detail, setDetail] = useState<ConfigTypeDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    fetchConfigDetail(code, paging.page, paging.pageSize, controller.signal)
      .then((result) => { setDetail(result); setError(null); })
      .catch((e: unknown) => { if (!controller.signal.aborted) setError(e instanceof Error ? e.message : "加载失败"); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [code, paging]);

  const columns: TableColumnsType<ConfigItem> = [
    { title: "编号", dataIndex: "id", width: 80 },
    { title: "名称", dataIndex: "name" },
    { title: "值", dataIndex: "value", ellipsis: { showTitle: true }, render: (value: string) => <span className="mono">{value}</span> },
    { title: "标签", dataIndex: "label", width: 80 },
    { title: "编码", dataIndex: "code", render: (value: string) => <span className="mono">{value}</span> },
    { title: "分组", dataIndex: "group_name", width: 100 },
    { title: "排序", dataIndex: "sort", width: 70 },
    { title: "备注信息", dataIndex: "remark", ellipsis: true },
    { title: "状态", dataIndex: "status", width: 90, render: (status: ConfigItem["status"]) => <StatusTag status={status} /> },
    {
      title: "操作",
      width: 200,
      render: () => (
        <Tooltip title={WRITE_HINT}>
          <span>
            <Button type="link" size="small" disabled>修改</Button>
            <Button type="link" size="small" disabled>禁用</Button>
            <Button type="link" size="small" disabled danger>删除</Button>
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

      <div className="config-toolbar">
        <Tooltip title={WRITE_HINT}><span><Button type="primary" disabled>新增</Button></span></Tooltip>
        <Tooltip title={WRITE_HINT}><span><Button disabled>修改</Button></span></Tooltip>
        <Tooltip title={WRITE_HINT}><span><Button disabled danger>删除</Button></span></Tooltip>
      </div>

      <Table<ConfigItem>
        rowKey="id"
        columns={columns}
        dataSource={detail?.items.rows ?? []}
        loading={loading}
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
    </div>
  );
}
