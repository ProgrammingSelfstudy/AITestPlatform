import { Alert, Button, Select, Table, Tag, Tooltip } from "antd";
import type { TableColumnsType } from "antd";
import { useEffect, useState } from "react";
import { NEED_ADMIN_HINT } from "./ConfigsPage";
import { fetchAccountIdentities, fetchPoolAccounts, setAccountIdentity, type AccountIdentityOption, type PoolAccount } from "./api";
import { useRememberedUserName } from "./identity";

const STATUS_TEXT: Record<PoolAccount["status"], { text: string; color: string }> = {
  idle: { text: "空闲", color: "green" },
  leased: { text: "使用中", color: "blue" },
  cooling: { text: "冷却中", color: "orange" },
  quarantined: { text: "已隔离", color: "red" },
};

/**
 * 参数配置 → 账号：给账号勾选身份。
 * 只显示昵称、环境、状态、绑定设备、身份，不显示用户名和密码；账号本身仍用命令行导入。
 * 身份选项只来自参数配置「账号身份」，只有管理员能改。
 */
export default function AccountsPage() {
  const { name: userName } = useRememberedUserName();
  const [accounts, setAccounts] = useState<PoolAccount[] | null>(null);
  const [identities, setIdentities] = useState<AccountIdentityOption[]>([]);
  const [canWrite, setCanWrite] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [saving, setSaving] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    const controller = new AbortController();
    Promise.all([fetchPoolAccounts(controller.signal), fetchAccountIdentities(userName.trim(), controller.signal)])
      .then(([rows, ids]) => {
        setAccounts(rows);
        setIdentities(ids.options);
        setCanWrite(ids.canWrite);
        setError(null);
      })
      .catch((e: unknown) => { if (!controller.signal.aborted) setError(e instanceof Error ? e.message : "加载失败"); });
    return () => controller.abort();
  }, [userName, reloadKey]);

  async function change(id: string, identity: string | null) {
    setSaving(id);
    setActionError(null);
    try {
      await setAccountIdentity(id, identity, userName.trim());
      setAccounts((rows) => rows?.map((r) => (r.id === id ? { ...r, identity } : r)) ?? rows);
    } catch (e) {
      setActionError(e instanceof Error ? e.message : "保存失败");
    } finally {
      setSaving(null);
    }
  }

  const columns: TableColumnsType<PoolAccount> = [
    { title: "项目", dataIndex: "project", width: 140 },
    { title: "环境", dataIndex: "env", width: 90 },
    { title: "昵称", dataIndex: "nickname" },
    { title: "绑定设备", dataIndex: "device_serial", render: (v: string | null) => v ?? "—" },
    { title: "状态", dataIndex: "status", width: 100, render: (s: PoolAccount["status"]) => <Tag color={STATUS_TEXT[s]?.color}>{STATUS_TEXT[s]?.text ?? s}</Tag> },
    {
      title: "身份",
      dataIndex: "identity",
      width: 260,
      render: (identity: string | null, row) => (
        <Tooltip title={canWrite ? undefined : NEED_ADMIN_HINT}>
          <span>
            <Select<string>
              aria-label={`账号 ${row.nickname} 的身份`}
              style={{ width: 220 }}
              value={identity ?? undefined}
              allowClear
              disabled={!canWrite || saving === row.id}
              placeholder={identities.length === 0 ? "还没有配置账号身份" : "不区分身份"}
              onChange={(v) => void change(row.id, v ?? null)}
              options={[
                ...identities.map((i) => ({ value: i.code, label: `${i.name}（${i.code}）` })),
                // 账号上已有的身份被停用或删除后仍要看得见，不能悄悄变成「不区分」
                ...(identity && !identities.some((i) => i.code === identity) ? [{ value: identity, label: `${identity}（已不在配置里）` }] : []),
              ]}
            />
          </span>
        </Tooltip>
      ),
    },
  ];

  return (
    <div>
      <Alert
        type="info"
        showIcon
        style={{ marginBottom: 12 }}
        message="身份选项在「参数配置 → 账号身份」里维护；用例选了身份，就只会领到该身份的账号。这里不显示账号密码。"
        action={<Button size="small" href="#/configs/account_identity">去维护身份</Button>}
      />
      {error && <Alert type="error" showIcon message={error} style={{ marginBottom: 12 }} />}
      {actionError && <Alert type="error" showIcon closable message={actionError} onClose={() => setActionError(null)} style={{ marginBottom: 12 }} />}
      <div style={{ marginBottom: 12 }}><Button onClick={() => setReloadKey((k) => k + 1)}>刷新</Button></div>
      <Table<PoolAccount> rowKey="id" size="middle" loading={accounts === null && !error} dataSource={accounts ?? []} columns={columns} pagination={{ pageSize: 20, hideOnSinglePage: true }} locale={{ emptyText: "还没有导入账号（用命令行 import-accounts 导入）" }} />
    </div>
  );
}
