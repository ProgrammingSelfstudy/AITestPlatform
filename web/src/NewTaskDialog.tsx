import { Alert, Form, Input, Modal, Select, Skeleton } from "antd";
import { useEffect, useRef, useState } from "react";
import { createTask, fetchCases, fetchModels, fetchNodes, type ModelRole, type Models, type ReviewStatus } from "./api";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";
import { LoadError } from "./runUi";
import IdentityHint from "./IdentityHint";
import { forgetUserName, NEED_LOGIN_MESSAGE, readUserName, rememberUserName, userNameExpiry } from "./identity";
import { REFRESH_MS, usePolling } from "./usePolling";

const project = "hafa";
const onceLoaded = () => false;

async function loadChoices(signal: AbortSignal) {
  const [cases, models] = await Promise.all([fetchCases(project, signal), fetchModels(signal)]);
  return { cases, models };
}

const REVIEW_LABEL: Record<ReviewStatus, string> = {
  draft: "草稿",
  reviewed_ok: "评审员认为没问题，待你确认",
  needs_human: "需要人工确认",
  approved: "已批准",
  rejected: "已拒绝",
};

const rolesOf = (models: Models | undefined, slot: ModelRole["slot"]) => models?.roles.filter((r) => r.slot === slot) ?? [];

interface Fields {
  node: string;
  cases: string[];
  executorRole: string;
  executor: string;
  effort?: string;
  judgeRole: string;
  judge: string;
  userName: string;
}

export default function NewTaskDialog({ onClose, initialCases }: { onClose: () => void; initialCases?: string[] }) {
  const [form] = Form.useForm<Fields>();
  const { data, error } = usePolling(loadChoices, REFRESH_MS, onceLoaded);
  const { data: nodes, error: nodeError } = usePolling(fetchNodes);
  const submitLock = useRef(false);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [pendingFields, setPendingFields] = useState<Fields | null>(null);
  const busy = submitting || pendingFields !== null;
  const model = Form.useWatch("executor", form);
  const node = Form.useWatch("node", form);
  const userName = Form.useWatch("userName", form) ?? "";
  const hasOnlineNodes = nodes?.some((candidate) => candidate.online);
  const nodeUnavailable = Boolean(node && nodes && !nodes.some((candidate) => candidate.node_id === node && candidate.online));
  const executorRoleId = Form.useWatch("executorRole", form);
  const judgeRoleId = Form.useWatch("judgeRole", form);
  const executorRoles = rolesOf(data?.models, "executor");
  const judgeRoles = rolesOf(data?.models, "judge");
  const executorRole = executorRoles.find((r) => r.id === executorRoleId);
  const judgeRole = judgeRoles.find((r) => r.id === judgeRoleId);
  const efforts = executorRole?.models.find((m) => m.model === model)?.efforts ?? [];

  // 配置加载后填默认：每个岗位的第一个角色和它的默认模型。只在首次加载时填，轮询刷新不覆盖用户已选的值
  useEffect(() => {
    if (!data) return;
    const executor = rolesOf(data.models, "executor")[0];
    const judge = rolesOf(data.models, "judge")[0];
    if (form.getFieldValue("executorRole")) return;
    // 从模块页进来时预选该模块已批准的用例
    if (initialCases?.length) form.setFieldValue("cases", initialCases);
    form.setFieldsValue({
      executorRole: executor?.id,
      executor: executor?.default.model,
      effort: executor?.default.effort ?? undefined,
      judgeRole: judge?.id,
      judge: judge?.default.model,
    });
  }, [data, form]);

  function changeExecutorRole(id: string) {
    const role = executorRoles.find((r) => r.id === id);
    form.setFieldsValue({ executor: role?.default.model, effort: role?.default.effort ?? undefined });
  }

  function changeJudgeRole(id: string) {
    form.setFieldValue("judge", judgeRoles.find((r) => r.id === id)?.default.model);
  }

  function changeModel(model: string) {
    const available = executorRole?.models.find((m) => m.model === model)?.efforts ?? [];
    const preferred = executorRole?.default.effort;
    form.setFieldValue("effort", preferred && available.includes(preferred) ? preferred : available[0]);
  }

  async function submit(fields: Fields, enteredToken?: string) {
    if (submitLock.current || (pendingFields && enteredToken === undefined)) return;
    const token = enteredToken ?? readReviewToken();
    const ready = Boolean(nodes?.some((candidate) => candidate.node_id === fields.node && candidate.online));
    const action = writeTokenAction(ready, token);
    if (action === "blocked") {
      setPendingFields(null);
      return;
    }
    setSubmitError(null);
    if (action === "prompt") {
      setPendingFields(fields);
      return;
    }
    if (enteredToken !== undefined) rememberReviewToken(enteredToken);
    submitLock.current = true;
    setPendingFields(null);
    setSubmitting(true);
    try {
      const task = await createTask({
        project,
        node_id: fields.node,
        case_codes: fields.cases,
        executor: { role: fields.executorRole, model: fields.executor, ...(fields.effort ? { effort: fields.effort } : {}) },
        judge: { role: fields.judgeRole, model: fields.judge },
      }, token, fields.userName.trim());
      rememberUserName(fields.userName);
      window.location.hash = `/tasks/${task.task_id}`;
      onClose();
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : String(err));
    } finally {
      submitLock.current = false;
      setSubmitting(false);
    }
  }

  return (
    <Modal
      open title="发起任务" okText={submitting ? "正在发起…" : "发起任务"} cancelText="返回"
      mask={{ closable: !busy }} keyboard={!busy} closable={!busy}
      onCancel={onClose} onOk={() => form.submit()} confirmLoading={submitting}
      cancelButtonProps={{ disabled: busy }}
      okButtonProps={{ disabled: busy || !data || !hasOnlineNodes || nodeUnavailable }}
    >
      <Form form={form} layout="vertical" disabled={busy} onFinish={(fields) => submit(fields)} className="new-task-form" initialValues={{ userName: readUserName() }}>
        <Alert
          type="info" title="执行规则" showIcon
          description="任务只能在发起人自己的节点上、用自己的模型来源执行；额度用完会暂停并通知，不会自动换计费方式。"
        />
        {error && <LoadError title="无法加载用例和模型" error={error} />}
        {nodeError && <LoadError title="无法加载节点" error={nodeError} />}
        {submitError && <Alert type="error" title="发起任务失败" description={<div className="wrap-text">{submitError}</div>} showIcon />}
        {!data && !error && <Skeleton active paragraph={{ rows: 5 }} />}
        {data && (
          <>
            <Form.Item
              name="node" label="执行节点"
              rules={[{ required: true, message: "请选择在线节点" }]}
              validateStatus={nodeUnavailable ? "warning" : undefined}
              help={nodeUnavailable ? "所选节点已离线或不可用，请选择在线节点。" : nodes && !hasOnlineNodes ? "当前没有在线节点，请先启动自己的执行节点。" : undefined}
            >
              <Select
                placeholder="选择在线节点" disabled={!nodes || busy}
                options={nodes?.map((candidate) => ({
                  value: candidate.node_id,
                  label: `${candidate.node_id.slice(0, 8)} · ${candidate.online ? `在线，${candidate.devices.length} 台设备` : "离线，无法接收任务"}`,
                  disabled: !candidate.online,
                })) ?? []}
              />
            </Form.Item>
            <Form.Item name="cases" label="用例" rules={[{ type: "array", required: true, min: 1, message: "请至少选择一个用例" }]}>
              <Select
                mode="multiple" placeholder="按编号或标题搜索用例" showSearch={{ optionFilterProp: "label" }}
                virtual={false} maxTagCount="responsive"
                optionRender={(option) => <span className="case-choice">{option.label}</span>}
                options={data.cases.map((c) => ({
                  value: c.code,
                  label: `${c.code} · ${c.title}${c.should_fail ? "（故意失败）" : ""}${c.status !== "active" ? `（受阻 · ${c.blocked_reason || "原因未填写"}）` : ""}${c.review_status !== "approved" ? `（${REVIEW_LABEL[c.review_status]}，不能执行）` : ""}`,
                  disabled: c.status !== "active" || c.review_status !== "approved",
                }))}
              />
            </Form.Item>
            {/* 同一岗位只配了一个角色时不显示角色下拉，但表单里仍带着角色 id */}
            <Form.Item name="executorRole" label="执行员角色" hidden={executorRoles.length < 2}>
              <Select onChange={changeExecutorRole} options={executorRoles.map((r) => ({ value: r.id, label: r.name }))} />
            </Form.Item>
            <div className="model-fields">
              <Form.Item name="executor" label="执行员模型" rules={[{ required: true, message: "请选择执行员模型" }]}>
                <Select onChange={changeModel} options={executorRole?.models.map((m) => ({ value: m.model, label: m.model })) ?? []} />
              </Form.Item>
              {efforts.length > 0 && (
                <Form.Item name="effort" label="推理强度" rules={[{ required: true, message: "请选择推理强度" }]}>
                  <Select options={efforts.map((effort) => ({ value: effort, label: effort }))} />
                </Form.Item>
              )}
            </div>
            <Form.Item name="judgeRole" label="判定员角色" hidden={judgeRoles.length < 2}>
              <Select onChange={changeJudgeRole} options={judgeRoles.map((r) => ({ value: r.id, label: r.name }))} />
            </Form.Item>
            <Form.Item name="judge" label="判定员模型" rules={[{ required: true, message: "请选择判定员模型" }]}>
              <Select options={judgeRole?.models.map((m) => ({ value: m.model, label: m.model })) ?? []} />
            </Form.Item>
            <Form.Item
              name="userName" label="我是谁" extra={<span>填你的名字，用来确认所选节点是你的（只能用自己的节点，花自己的额度）。<IdentityHint
                userName={userName} expiresAt={userNameExpiry()} nameProblem={submitError}
                onForget={() => { forgetUserName(); form.setFieldValue("userName", ""); setSubmitError(null); }}
                onCreated={(n) => form.setFieldValue("userName", n)} onLogin={() => setSubmitError(null)} /></span>}
              rules={[{ required: true, whitespace: true, message: "请填「我是谁」（你的名字）" }]}
            >
              <Input autoComplete="off" maxLength={40} onChange={() => {
                if (submitError === NEED_LOGIN_MESSAGE) setSubmitError(null);
              }} />
            </Form.Item>
          </>
        )}
      </Form>
      {pendingFields && (
        <ReviewTokenDialog onConfirm={(token) => submit(pendingFields, token)} onCancel={() => setPendingFields(null)} />
      )}
    </Modal>
  );
}
