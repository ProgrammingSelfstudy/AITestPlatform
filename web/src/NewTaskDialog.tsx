import { Alert, Form, Input, Modal, Select, Skeleton } from "antd";
import { useEffect, useRef, useState } from "react";
import { createTask, fetchCases, fetchFlowTemplates, fetchModels, fetchNodeRoles, fetchNodes, fetchRoleLimits, ROLE_SLOT_LIMIT, type FlowTemplate, type ModelRole, type ReviewStatus, type RoleView } from "./api";
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


interface Fields {
  node: string;
  cases: string[];
  executorRole: string;
  executor: string;
  effort?: string;
  judgeRole: string;
  judge: string;
  /** 其他判定员（角色 id）：和上面的判定员同时评审同一份证据，各自出结论 */
  moreJudges?: string[];
  userName: string;
}

export default function NewTaskDialog({ onClose, initialCases, initialNode }: { onClose: () => void; initialCases?: string[]; initialNode?: string }) {
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
  // 从任务详情一键重跑时，若原执行节点仍在线则默认选中（只填一次，不覆盖用户手动切换）
  useEffect(() => {
    if (!initialNode || !nodes || form.getFieldValue("node")) return;
    if (nodes.some((candidate) => candidate.node_id === initialNode && candidate.online)) {
      form.setFieldValue("node", initialNode);
    }
  }, [initialNode, nodes, form]);
  const executorRoleId = Form.useWatch("executorRole", form);
  const judgeRoleId = Form.useWatch("judgeRole", form);
  // 判定员最多同时几个：读参数配置里的岗位上限，读不到用默认
  const [maxJudges, setMaxJudges] = useState(ROLE_SLOT_LIMIT);
  useEffect(() => {
    const controller = new AbortController();
    void fetchRoleLimits(controller.signal).then((limits) => { if (!controller.signal.aborted && limits.judge) setMaxJudges(limits.judge); });
    return () => controller.abort();
  }, []);
  // 角色从哪来：所选节点上报了角色（新版节点）就只能用它上面的、检测通过的角色——服务端也是这么校验的；
  // 节点没上报（旧版节点）才用 models.json 里的
  const [nodeRoles, setNodeRoles] = useState<RoleView[]>([]);
  const [nodeRolesError, setNodeRolesError] = useState<string | null>(null);
  const trimmedName = userName.trim();
  useEffect(() => {
    setNodeRoles([]);
    setNodeRolesError(null);
    if (!node || !trimmedName) return;
    const controller = new AbortController();
    fetchNodeRoles(node, trimmedName, controller.signal)
      .then((result) => {
        if (controller.signal.aborted) return;
        setNodeRoles(result.online ? result.roles : []);
        if (result.name_problem) setNodeRolesError(result.name_problem);
      })
      .catch((e: unknown) => { if (!controller.signal.aborted) setNodeRolesError(e instanceof Error ? e.message : "读取节点角色失败"); });
    return () => controller.abort();
  }, [node, trimmedName]);
  const usingNodeRoles = nodeRoles.length > 0;
  const pool: ModelRole[] = usingNodeRoles ? nodeRoles : (data?.models.roles ?? []);
  const isUsable = (role: ModelRole) => !usingNodeRoles || (role as RoleView).check?.status === "ok";
  const roleLabel = (role: ModelRole) => (isUsable(role) ? role.name : `${role.name}（还没通过检测）`);
  const poolOf = (slot: ModelRole["slot"]) => pool.filter((r) => r.slot === slot);
  const executorRoles = poolOf("executor");
  const judgeRoles = poolOf("judge");
  const executorRole = executorRoles.find((r) => r.id === executorRoleId);
  const judgeRole = judgeRoles.find((r) => r.id === judgeRoleId);
  const efforts = executorRole?.models.find((m) => m.model === model)?.efforts ?? [];

  // 配置加载后填默认：每个岗位的第一个角色和它的默认模型。只在首次加载时填，轮询刷新不覆盖用户已选的值
  const poolKey = pool.map((r) => `${r.id}:${(r as RoleView).check?.status ?? ""}`).join("|");
  useEffect(() => {
    if (!data) return;
    // 从模块页进来时预选该模块已批准的用例（只填一次）
    if (initialCases?.length && !form.getFieldValue("cases")) form.setFieldValue("cases", initialCases);
    // 已选的角色还在可用的角色里就保留用户的选择，否则换成第一个可用的
    const keep = (field: "executorRole" | "judgeRole", list: ModelRole[]) => list.some((r) => r.id === form.getFieldValue(field) && isUsable(r));
    const executor = executorRoles.find(isUsable);
    const judge = judgeRoles.find(isUsable);
    if (!keep("executorRole", executorRoles)) {
      form.setFieldsValue({ executorRole: executor?.id, executor: executor?.default.model, effort: executor?.default.effort ?? undefined });
    }
    if (!keep("judgeRole", judgeRoles)) {
      form.setFieldsValue({ judgeRole: judge?.id, judge: judge?.default.model });
    }
    const more: string[] = form.getFieldValue("moreJudges") ?? [];
    if (more.length) form.setFieldValue("moreJudges", more.filter((id) => judgeRoles.some((r) => r.id === id && isUsable(r))));
  }, [data, form, poolKey]);

  // 流程（参数配置里的「流程模板」）：选了流程，按角色名在这个节点上自动选好执行员、判定员和其他判定员
  const [flows, setFlows] = useState<FlowTemplate[]>([]);
  const [flowCode, setFlowCode] = useState("");
  const [flowNotes, setFlowNotes] = useState<string[]>([]);
  useEffect(() => {
    const controller = new AbortController();
    void fetchFlowTemplates(controller.signal).then((list) => { if (!controller.signal.aborted) { setFlows(list); setFlowCode((prev) => prev || list[0]?.code || ""); } });
    return () => controller.abort();
  }, []);
  useEffect(() => {
    const flow = flows.find((f) => f.code === flowCode);
    if (!flow || !usingNodeRoles) {
      setFlowNotes([]);
      return;
    }
    const notes: string[] = [];
    // 按流程里写的顺序，取在这个节点上存在且检测通过的；不存在或没通过的逐个说明
    const resolve = (slot: "executor" | "judge", label: string, list: ModelRole[]): ModelRole[] => {
      const found: ModelRole[] = [];
      for (const name of flow.roles[slot] ?? []) {
        const role = list.find((r) => r.name === name);
        if (role && isUsable(role)) found.push(role);
        else notes.push(`流程里的${label}「${name}」${role ? "还没通过检测" : "在这个节点上不存在"}`);
      }
      return found;
    };
    const executors = resolve("executor", "执行员", executorRoles);
    const judges = resolve("judge", "判定员", judgeRoles);
    if (!flow.roles.executor) notes.push("这个流程没有指定执行员，请手动选");
    if (!flow.roles.judge) notes.push("这个流程没有指定判定员，请手动选");
    if (executors[0]) form.setFieldsValue({ executorRole: executors[0].id, executor: executors[0].default.model, effort: executors[0].default.effort ?? undefined });
    if (judges[0]) {
      // 多个判定员：第一个是主判定员，其余同时评审；个数不超过判定员岗位上限
      const extras = judges.slice(1, Math.max(1, maxJudges));
      if (judges.length - 1 > extras.length) notes.push(`判定员最多同时 ${maxJudges} 个，流程里多出来的没有选`);
      form.setFieldsValue({ judgeRole: judges[0].id, judge: judges[0].default.model, moreJudges: extras.map((r) => r.id) });
    }
    setFlowNotes(notes);
  }, [flows, flowCode, poolKey, maxJudges]);

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
        ...(fields.moreJudges?.length
          ? { more_judges: fields.moreJudges.map((id) => ({ role: id, model: judgeRoles.find((r) => r.id === id)?.default.model ?? "" })) }
          : {}),
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
            {flows.length > 0 && usingNodeRoles && (
              <Form.Item label="流程" extra={flowNotes.length > 0 && flowCode ? `${flowNotes.join("；")}。` : "选了流程，下面的执行员、判定员按流程里的角色自动选好（仍可手动改）。流程在「参数配置」里配置。"}>
                <Select
                  aria-label="流程"
                  value={flowCode || undefined}
                  placeholder="选择流程"
                  options={[...flows.map((f) => ({ value: f.code, label: f.name })), { value: "", label: "不用流程，手动选角色" }]}
                  onChange={(code) => setFlowCode(code)}
                />
              </Form.Item>
            )}
            {nodeRolesError && <Alert type="warning" showIcon title={nodeRolesError} />}
            {node && trimmedName && !usingNodeRoles && !nodeRolesError && (
              <Alert type="info" showIcon title="没有读到这个节点上的角色，先用配置文件里的角色（旧版节点）；如果节点已经配了角色，请先在「角色与模型」页配置并检测。" />
            )}
            {/* 同一岗位只配了一个角色时不显示角色下拉，但表单里仍带着角色 id */}
            <Form.Item name="executorRole" label="执行员角色" hidden={executorRoles.length < 2}>
              <Select onChange={changeExecutorRole} options={executorRoles.map((r) => ({ value: r.id, label: roleLabel(r), disabled: !isUsable(r) }))} />
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
              <Select onChange={changeJudgeRole} options={judgeRoles.map((r) => ({ value: r.id, label: roleLabel(r), disabled: !isUsable(r) }))} />
            </Form.Item>
            <Form.Item name="judge" label="判定员模型" rules={[{ required: true, message: "请选择判定员模型" }]}>
              <Select options={judgeRole?.models.map((m) => ({ value: m.model, label: m.model })) ?? []} />
            </Form.Item>
            {judgeRoles.length > 1 && (
              <Form.Item
                name="moreJudges"
                label="其他判定员（可选，同时评审）"
                extra={`多选几个不同的判定员，他们和上面的判定员同时评审同一份证据，各自出结论，结果分别记录（用各自角色的默认模型）。都完成且结论一致才采用，有分歧就转待人工确认。判定员最多同时 ${maxJudges} 个（在「参数配置」里设置）。`}
                rules={[{ type: "array", max: Math.max(0, maxJudges - 1), message: `最多再选 ${Math.max(0, maxJudges - 1)} 个（含上面的判定员一共不超过 ${maxJudges} 个）` }]}
              >
                <Select
                  mode="multiple"
                  allowClear
                  placeholder="不选就是只有一个判定员"
                  options={judgeRoles.filter((r) => r.id !== judgeRoleId).map((r) => ({ value: r.id, label: `${roleLabel(r)}（${r.default.model}）`, disabled: !isUsable(r) }))}
                />
              </Form.Item>
            )}
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
