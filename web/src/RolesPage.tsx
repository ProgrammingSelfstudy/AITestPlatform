import { Alert, Button, Empty, Form, Input, Modal, Segmented, Select, Skeleton } from "antd";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  checkNodeRole,
  fetchNodeRoles,
  fetchNodes,
  initNodeRoles,
  ROLE_SOURCES,
  saveNodeRoles,
  releaseNode,
  forceReleaseNode,
  claimNode,
  type CheckStatus,
  type ModelRole,
  type NodeRoles,
  type NodeView,
  type RoleInput,
  type RoleView,
} from "./api";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import AppsPanel from "./AppsPanel";
import IdentityHint from "./IdentityHint";
import { NEED_LOGIN_MESSAGE, useRememberedUserName } from "./identity";
import ReviewTokenDialog from "./ReviewTokenDialog";
import { formatDate, LoadError } from "./runUi";
import { REFRESH_MS, usePolling } from "./usePolling";
import "./roles.css";

type StatusTone = "pass" | "run" | "que" | "unc" | "lost" | "fail";

interface CheckMeta {
  text: string;
  tone: StatusTone;
  available: boolean;
}

// 检测状态文案与语义色：只有 ok 才是绿色，失败/未登录/额度用完绝不显示绿色
const CHECK_META: Record<CheckStatus, CheckMeta> = {
  unchecked: { text: "未检测", tone: "que", available: false },
  checking: { text: "检测中", tone: "run", available: false },
  ok: { text: "可用", tone: "pass", available: true },
  not_logged_in: { text: "未登录", tone: "unc", available: false },
  quota_exhausted: { text: "额度用完", tone: "lost", available: false },
  failed: { text: "检测失败", tone: "fail", available: false },
};

const SLOT_LABEL: Record<ModelRole["slot"], string> = {
  executor: "执行员",
  judge: "判定员",
  case_writer: "测试用例",
  planner: "管理员",
};

interface ModelRowField {
  model: string;
  efforts: string[];
}

interface RoleFormValues {
  name: string;
  slot: ModelRole["slot"];
  source: string;
  models: ModelRowField[];
  defaultModel: string;
  defaultEffort?: string;
  apiKey?: string;
}

interface SourceOption {
  value: string;
  label: string;
  disabled: boolean;
  hint: string;
  rawLabel: string;
}

type RolesPollState =
  | { status: "idle" }
  | { status: "loaded"; nodeId: string; userName: string; payload: NodeRoles };

const IDLE_POLL: RolesPollState = { status: "idle" };

type WriteIntent =
  | { kind: "save"; payload: RoleInput[]; createdIds: string[] }
  | { kind: "init" }
  | { kind: "release" }
  | { kind: "check"; roleId: string; roleName: string }
  | { kind: "delete"; roleId: string; roleName: string; payload: RoleInput[] };

function isAbnormalStatus(status: CheckStatus): boolean {
  return status === "failed" || status === "not_logged_in" || status === "quota_exhausted";
}

function allowedSlotsForSource(source: string): ModelRole["slot"][] {
  if (source === "codex_exec") return ["executor", "case_writer", "planner"];
  if (source === "claude_cli") return ["judge", "case_writer", "planner"];
  if (source === "agy") return ["judge", "planner"];
  return ["executor", "judge"];
}

function findSourceMeta(source: string) {
  return ROLE_SOURCES.find((item) => item.value === source);
}

function sourceLabel(source: string): string {
  return findSourceMeta(source)?.label ?? source;
}

function sourceNeedsKey(source: string): boolean {
  return Boolean(findSourceMeta(source)?.needsKey);
}

function toRoleInput(role: ModelRole): RoleInput {
  return {
    id: role.id,
    slot: role.slot,
    name: role.name,
    source: role.source,
    default: { model: role.default.model, effort: role.default.effort },
    models: role.models.map((item) => ({ model: item.model, efforts: [...item.efforts] })),
  };
}

function roleToFormValues(role: ModelRole): RoleFormValues {
  return {
    name: role.name,
    slot: role.slot,
    source: role.source,
    models: role.models.length > 0
      ? role.models.map((item) => ({ model: item.model, efforts: [...item.efforts] }))
      : [{ model: "", efforts: [] }],
    defaultModel: role.default.model,
    defaultEffort: role.default.effort ?? undefined,
    apiKey: "",
  };
}

function configSignature(role: ModelRole): string {
  return JSON.stringify(toRoleInput(role));
}

function generateRoleId(existingIds: Set<string>, source: string): string {
  const base = source.replace(/[^a-z0-9_-]/gi, "") || "role";
  let index = 1;
  while (existingIds.has(`${base}_${index}`)) {
    index += 1;
  }
  return `${base}_${index}`;
}

function buildRoleFromForm(id: string, values: RoleFormValues): RoleInput {
  const cleanedModels: { model: string; efforts: string[] }[] = [];
  const seenModels = new Set<string>();
  for (const row of values.models ?? []) {
    const modelName = (row?.model ?? "").trim();
    if (!modelName || seenModels.has(modelName)) continue;
    seenModels.add(modelName);
    const efforts = Array.from(
      new Set((row?.efforts ?? []).map((item) => item.trim()).filter(Boolean)),
    );
    cleanedModels.push({ model: modelName, efforts });
  }

  const allowed = allowedSlotsForSource(values.source);
  const slot = allowed.includes(values.slot) ? values.slot : allowed[0];
  const defaultModelName = (values.defaultModel ?? "").trim() || cleanedModels[0]?.model || "";
  const matchedModel = cleanedModels.find((item) => item.model === defaultModelName);
  const rawEffort = (values.defaultEffort ?? "").trim();
  const defaultEffort =
    matchedModel && matchedModel.efforts.length > 0
      ? (rawEffort && matchedModel.efforts.includes(rawEffort) ? rawEffort : matchedModel.efforts[0])
      : null;

  const editedApiKey = sourceNeedsKey(values.source) ? (values.apiKey ?? "").trim() : "";

  return {
    id,
    slot,
    name: values.name.trim(),
    source: values.source,
    default: { model: defaultModelName, effort: defaultEffort },
    models: cleanedModels,
    ...(editedApiKey ? { api_key: editedApiKey } : {}),
  };
}

function validateFormDraft(values: RoleFormValues): string | null {
  if (!values.name || !values.name.trim()) return "请填写角色名称";
  if (!values.source) return "请选择模型来源";
  const allowed = allowedSlotsForSource(values.source);
  if (!allowed.includes(values.slot)) {
    return `${sourceLabel(values.source)} 仅支持担任${allowed.map((s) => SLOT_LABEL[s]).join("或")}`;
  }
  const rows = values.models ?? [];
  const names = rows.map((row) => (row?.model ?? "").trim()).filter(Boolean);
  if (names.length === 0) return "请至少添加一个有效模型";
  if (new Set(names).size !== names.length) return "模型列表中存在重复的模型名称";
  const defModel = (values.defaultModel ?? "").trim();
  if (!defModel || !names.includes(defModel)) return "请选择模型列表中的一项作为默认模型";
  return null;
}

function StatusBadge({ status }: { status: CheckStatus }) {
  const meta = CHECK_META[status];
  return (
    <span className={`roles-status-badge roles-tone-${meta.tone}`}>
      <span
        className={`roles-status-dot${status === "checking" ? " roles-checking-dot" : ""}`}
        aria-hidden="true"
      />
      {meta.text}
    </span>
  );
}

function SlotIcon({ slot }: { slot: ModelRole["slot"] }) {
  if (slot === "executor") {
    return (
      <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
        <path
          d="M9.5 1.75L3.5 9h4.25L6.5 14.25 12.5 7H8.25L9.5 1.75z"
          stroke="currentColor"
          strokeWidth="1.4"
          strokeLinejoin="round"
        />
      </svg>
    );
  }
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M8 1.75l5 2.1v3.9c0 3.15-2.1 5.65-5 6.5-2.9-.85-5-3.35-5-6.5v-3.9l5-2.1z"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinejoin="round"
      />
      <path
        d="M5.75 8.1l1.55 1.55 3.1-3.3"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function KeyIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="5.5" cy="8" r="3" stroke="currentColor" strokeWidth="1.5" />
      <path d="M8.5 8H14.5M12 8v2.2M14 8v1.6" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
    </svg>
  );
}

function pipeAvailClass(status: CheckStatus): string {
  switch (status) {
    case "ok":
      return "is-ok";
    case "checking":
      return "is-run";
    case "unchecked":
      return "is-pending";
    case "not_logged_in":
      return "is-warn";
    case "quota_exhausted":
      return "is-lost";
    case "failed":
      return "is-bad";
  }
}

export default function RolesPage() {
  const [form] = Form.useForm<RoleFormValues>();

  // 节点与「我是谁」（只记名字、靠自觉；在本浏览器记 30 天；API Key 仍不落本地存储）。
  // 数据是公开的，谁都能看；只有节点主人能改、能检测（花他自己的额度）
  const [selectedNodeId, setSelectedNodeId] = useState<string>("");
  const { name: userName, setName: setUserName, expiresAt: nameExpiresAt, forget: forgetName } = useRememberedUserName();
  const trimmedName = userName.trim();

  // 节点列表轮询：数据未变时复用旧引用，避免无意义重渲染
  const nodesCacheRef = useRef<{ sig: string; data: NodeView[] } | null>(null);
  const loadNodes = useCallback(async (signal: AbortSignal): Promise<NodeView[]> => {
    const next = await fetchNodes(signal);
    const sig = JSON.stringify(next);
    if (nodesCacheRef.current && nodesCacheRef.current.sig === sig) {
      return nodesCacheRef.current.data;
    }
    nodesCacheRef.current = { sig, data: next };
    return next;
  }, []);
  const { data: nodes, error: nodesError } = usePolling(loadNodes, REFRESH_MS);

  const onlineNodes = useMemo(
    () => (nodes ?? []).filter((candidate) => candidate.online),
    [nodes],
  );

  // 首次检测到在线节点时默认选中第一个在线节点（不覆盖用户已选）
  useEffect(() => {
    if (!selectedNodeId && onlineNodes.length > 0) {
      setSelectedNodeId(onlineNodes[0].node_id);
    }
  }, [onlineNodes, selectedNodeId]);

  // 角色列表轮询：未选节点时不发请求（名字可以不填，只读查看）；数据未变不触发重渲染或骨架屏闪烁
  const rolesCacheRef = useRef<{ key: string; sig: string; value: RolesPollState } | null>(null);
  const loadRoles = useCallback(
    async (signal: AbortSignal): Promise<RolesPollState> => {
      if (!selectedNodeId) {
        return IDLE_POLL;
      }
      const payload = await fetchNodeRoles(selectedNodeId, trimmedName, signal);
      const key = `${selectedNodeId}\0${trimmedName}`;
      const sig = JSON.stringify(payload);
      if (
        rolesCacheRef.current &&
        rolesCacheRef.current.key === key &&
        rolesCacheRef.current.sig === sig
      ) {
        return rolesCacheRef.current.value;
      }
      const value: RolesPollState = { status: "loaded", nodeId: selectedNodeId, userName: trimmedName, payload };
      rolesCacheRef.current = { key, sig, value };
      return value;
    },
    [selectedNodeId, trimmedName],
  );
  const keepPollingRoles = useCallback((state: RolesPollState) => state.status === "loaded", []);
  const { data: rolesPoll, error: rolesError, reload: reloadRoles } = usePolling(
    loadRoles,
    REFRESH_MS,
    keepPollingRoles,
  );

  const canQueryRoles = Boolean(selectedNodeId);
  const loadedForNode = rolesPoll?.status === "loaded" && rolesPoll.nodeId === selectedNodeId;
  const selectedNodeRecord = useMemo(
    () => nodes?.find((candidate) => candidate.node_id === selectedNodeId) ?? null,
    [nodes, selectedNodeId],
  );
  const isNodeOffline = Boolean(
    (loadedForNode && !rolesPoll.payload.online) ||
    (selectedNodeId && selectedNodeRecord && !selectedNodeRecord.online),
  );
  const serverRoles: RoleView[] = useMemo(
    () => (loadedForNode && rolesPoll.payload.online ? rolesPoll.payload.roles : []),
    [loadedForNode, rolesPoll],
  );
  const isInitialLoading = canQueryRoles && !loadedForNode && !rolesError && !isNodeOffline;
  // 名字变动时等这个名字的结果再判断是否是节点主人。
  const viewer = loadedForNode && rolesPoll.userName === trimmedName ? rolesPoll.payload : null;
  const mine = viewer?.mine ?? null;
  const canAct = Boolean(trimmedName) && mine === true && !isNodeOffline;
  const readOnlyReason: string | undefined = !selectedNodeId
    ? "先在上面选一个在线节点"
    : canAct
      ? undefined
      : !trimmedName
        ? "你现在是只读：在上面填「我是谁」（你的名字）后，才能检查本机环境、创建和修改角色（只能改自己的节点）"
        : viewer?.mine === false
          ? (viewer.name_problem ?? "这是别人的节点：你能看，但只能修改和使用自己的节点（用的是各人自己的额度）")
          : undefined;
  const readOnly = !canAct;

  // 选中角色、本地未保存草稿、新增草稿
  const [selectedRoleId, setSelectedRoleId] = useState<string | null>(null);
  // 新建但还没保存的角色可以同时有多个（比如先建执行员、再建判定员，一起保存）
  const [draftNewRoles, setDraftNewRoles] = useState<ModelRole[]>([]);
  const [draftsById, setDraftsById] = useState<Record<string, RoleFormValues>>({});

  // 写操作与审核口令流程（与 NewTaskDialog / ScriptsPage 一致）
  const submitLock = useRef(false);
  const [busyAction, setBusyAction] = useState<WriteIntent["kind"] | null>(null);
  const [pendingWrite, setPendingWrite] = useState<WriteIntent | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<{ id: string; name: string; isDraftNew: boolean } | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [actionNotice, setActionNotice] = useState<string | null>(null);
  const [adminOpen, setAdminOpen] = useState(false);
  const [adminName, setAdminName] = useState("");
  const [adminPassword, setAdminPassword] = useState("");
  const [adminBusy, setAdminBusy] = useState(false);

  const [claimBusy, setClaimBusy] = useState(false);

  async function claimThisNode() {
    if (!selectedNodeId || claimBusy) return;
    setClaimBusy(true);
    setActionError(null);
    try {
      await claimNode(selectedNodeId, trimmedName);
      setActionNotice("已认领这个节点：现在它是你的，可以选应用、创建和检测角色了（用的是你自己的额度）。");
      reloadRoles();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setClaimBusy(false);
    }
  }

  async function forceRelease() {
    if (!selectedNodeId || adminBusy) return;
    setAdminBusy(true);
    setActionError(null);
    try {
      await forceReleaseNode(selectedNodeId, adminName.trim(), adminPassword);
      setAdminOpen(false);
      setAdminPassword("");
      setActionNotice("管理员已强制解除绑定：这个节点现在没有主人，创建名字时可以认领它。节点若仍用原来的 NODE_USER_NAME 启动，重启后会再绑回去。");
      reloadRoles();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setAdminBusy(false);
    }
  }

  const busy = busyAction !== null;

  // 切换节点或换了名字时立即清理草稿与临时状态（避免残留任何未保存内容或 Key）
  useEffect(() => {
    setSelectedRoleId(null);
    setDraftNewRoles([]);
    setDraftsById({});
    setPendingWrite(null);
    setDeleteTarget(null);
    setActionError(null);
    setActionNotice(null);
    form.resetFields();
  }, [selectedNodeId, trimmedName, form]);

  // 合并服务端角色与本地未保存新建角色，方便左栏与编辑区统一渲染
  const allDisplayRoles: RoleView[] = useMemo(() => {
    const list = serverRoles.map((role) => {
      const draft = draftsById[role.id];
      if (!draft) return role;
      const input = buildRoleFromForm(role.id, draft);
      return {
        ...role,
        name: input.name || role.name,
        slot: input.slot,
        source: input.source,
        default: input.default,
        models: input.models,
      };
    });
    for (const draftRole of draftNewRoles) {
      if (serverRoles.some((role) => role.id === draftRole.id)) continue;
      const draft = draftsById[draftRole.id];
      const base = draft ? buildRoleFromForm(draftRole.id, draft) : draftRole;
      list.push({
        id: base.id,
        slot: base.slot,
        name: base.name || "未命名新角色",
        source: base.source,
        default: base.default,
        models: base.models,
        key_configured: false,
        check: { status: "unchecked", detail: null, checked_at: null },
      });
    }
    return list;
  }, [serverRoles, draftNewRoles, draftsById]);

  // 新建角色保存后，待服务端轮询返回该角色时再平滑移除本地新建草稿，避免瞬间跳回第一项
  useEffect(() => {
    if (draftNewRoles.some((draftRole) => serverRoles.some((role) => role.id === draftRole.id))) {
      setDraftNewRoles((prev) => prev.filter((draftRole) => !serverRoles.some((role) => role.id === draftRole.id)));
    }
  }, [draftNewRoles, serverRoles]);

  // 自动选中第一个角色；若当前选中角色已不存在则回退
  useEffect(() => {
    if (!loadedForNode || isNodeOffline) return;
    if (selectedRoleId && allDisplayRoles.some((item) => item.id === selectedRoleId)) {
      return;
    }
    if (allDisplayRoles.length > 0) {
      setSelectedRoleId(allDisplayRoles[0].id);
    } else {
      setSelectedRoleId(null);
    }
  }, [loadedForNode, isNodeOffline, selectedRoleId, allDisplayRoles]);

  const activeServerRole = useMemo(
    () => serverRoles.find((item) => item.id === selectedRoleId) ?? null,
    [serverRoles, selectedRoleId],
  );
  const activeDraftRole = draftNewRoles.find((draftRole) => draftRole.id === selectedRoleId) ?? null;
  const isEditingNewDraft = Boolean(activeDraftRole && !activeServerRole);
  const activeRoleForForm: ModelRole | null = isEditingNewDraft ? activeDraftRole : activeServerRole;
  const activeConfigSig = activeRoleForForm ? configSignature(activeRoleForForm) : "";

  // 仅在切换角色或已保存配置真正变更（且无本地草稿）时同步到表单，绝不在轮询更新检测状态时打断用户输入
  const lastSyncedRef = useRef<{ roleId: string | null; sig: string }>({ roleId: null, sig: "" });
  useEffect(() => {
    if (!selectedRoleId || !activeRoleForForm) {
      lastSyncedRef.current = { roleId: null, sig: "" };
      form.resetFields();
      return;
    }
    const existingDraft = draftsById[selectedRoleId];
    if (lastSyncedRef.current.roleId === selectedRoleId) {
      if (existingDraft || lastSyncedRef.current.sig === activeConfigSig) {
        return;
      }
    }
    form.resetFields();
    form.setFieldsValue(existingDraft ?? roleToFormValues(activeRoleForForm));
    lastSyncedRef.current = { roleId: selectedRoleId, sig: activeConfigSig };
  }, [selectedRoleId, activeRoleForForm, activeConfigSig, draftsById, form]);

  // 监听表单字段，驱动岗位限制、默认模型/强度联动、API Key 条件显示
  const watchedSource = Form.useWatch("source", form) ?? activeRoleForForm?.source ?? "codex_exec";
  const watchedModels = Form.useWatch("models", form);
  const watchedDefaultModel = Form.useWatch("defaultModel", form) ?? "";

  const allowedSlots = useMemo(() => allowedSlotsForSource(watchedSource), [watchedSource]);
  const selectedSourceMeta = useMemo(() => findSourceMeta(watchedSource), [watchedSource]);
  const showApiKeyField = Boolean(selectedSourceMeta?.needsKey);

  const availableModelNames = useMemo(() => {
    const names: string[] = [];
    const seen = new Set<string>();
    for (const row of watchedModels ?? []) {
      const trimmed = (row?.model ?? "").trim();
      if (trimmed && !seen.has(trimmed)) {
        seen.add(trimmed);
        names.push(trimmed);
      }
    }
    return names;
  }, [watchedModels]);

  const defaultModelEfforts = useMemo(() => {
    const target = watchedDefaultModel.trim();
    const matched = (watchedModels ?? []).find((row) => (row?.model ?? "").trim() === target);
    if (!matched) return [];
    return Array.from(new Set((matched.efforts ?? []).map((item) => item.trim()).filter(Boolean)));
  }, [watchedModels, watchedDefaultModel]);

  // 统计数据（全部来自服务端真实 roles）
  const stats = useMemo(() => {
    let ok = 0;
    let unchecked = 0;
    let checking = 0;
    let abnormal = 0;
    let executors = 0;
    let judges = 0;
    let executorsOk = 0;
    let judgesOk = 0;

    for (const role of serverRoles) {
      if (role.slot === "executor") {
        executors += 1;
        if (role.check.status === "ok") executorsOk += 1;
      } else {
        judges += 1;
        if (role.check.status === "ok") judgesOk += 1;
      }
      if (role.check.status === "ok") ok += 1;
      else if (role.check.status === "unchecked") unchecked += 1;
      else if (role.check.status === "checking") checking += 1;
      else if (isAbnormalStatus(role.check.status)) abnormal += 1;
    }

    return {
      total: serverRoles.length,
      ok,
      unchecked,
      checking,
      abnormal,
      executors,
      judges,
      executorsOk,
      judgesOk,
    };
  }, [serverRoles]);

  const executorRoles = useMemo(
    () => serverRoles.filter((role) => role.slot === "executor"),
    [serverRoles],
  );
  const judgeRoles = useMemo(
    () => serverRoles.filter((role) => role.slot === "judge"),
    [serverRoles],
  );

  // 检测日志：只展示 roles 里真实有检测记录或正在检测的角色，绝不编造
  const checkLogEntries = useMemo(() => {
    return serverRoles
      .filter(
        (role) =>
          role.check.status !== "unchecked" ||
          role.check.checked_at !== null ||
          Boolean(role.check.detail),
      )
      .sort((left, right) => {
        if (left.check.status === "checking" && right.check.status !== "checking") return -1;
        if (right.check.status === "checking" && left.check.status !== "checking") return 1;
        const leftTs = left.check.checked_at ? Date.parse(left.check.checked_at) : 0;
        const rightTs = right.check.checked_at ? Date.parse(right.check.checked_at) : 0;
        return rightTs - leftTs;
      });
  }, [serverRoles]);

  const dirtyCount = Object.keys(draftsById).length + draftNewRoles.filter((draftRole) => !draftsById[draftRole.id]).length;

  // ---------------------------------------------------------------- 表单联动与草稿维护

  function handleFormValuesChange(_changed: Partial<RoleFormValues>, allValues: RoleFormValues) {
    if (!selectedRoleId) return;
    setActionError(null);
    setActionNotice(null);
    let syncedValues = allValues;
    if (allValues.defaultEffort) {
      const target = (allValues.defaultModel ?? "").trim();
      const matched = (allValues.models ?? []).find((row) => (row?.model ?? "").trim() === target);
      const validEfforts = Array.from(
        new Set((matched?.efforts ?? []).map((item) => item.trim()).filter(Boolean)),
      );
      if (!validEfforts.includes(allValues.defaultEffort)) {
        const nextEffort = validEfforts[0];
        form.setFieldValue("defaultEffort", nextEffort);
        syncedValues = { ...allValues, defaultEffort: nextEffort };
      }
    }
    setDraftsById((prev) => ({ ...prev, [selectedRoleId]: syncedValues }));
  }

  function handleSourceChange(nextSource: string) {
    const allowed = allowedSlotsForSource(nextSource);
    const currentSlot = form.getFieldValue("slot") as ModelRole["slot"] | undefined;
    const patch: Partial<RoleFormValues> = {};
    if (!currentSlot || !allowed.includes(currentSlot)) {
      patch.slot = allowed[0];
    }
    if (!sourceNeedsKey(nextSource)) {
      patch.apiKey = "";
    }
    if (Object.keys(patch).length > 0) {
      form.setFieldsValue(patch);
    }
    if (selectedRoleId) {
      const merged = { ...form.getFieldsValue(true), source: nextSource, ...patch } as RoleFormValues;
      setDraftsById((prev) => ({ ...prev, [selectedRoleId]: merged }));
    }
  }

  function handleDefaultModelChange(nextModel: string) {
    const rows = (form.getFieldValue("models") as ModelRowField[] | undefined) ?? [];
    const matched = rows.find((row) => (row?.model ?? "").trim() === nextModel);
    const nextEfforts = Array.from(
      new Set((matched?.efforts ?? []).map((item) => item.trim()).filter(Boolean)),
    );
    const currentEffort = form.getFieldValue("defaultEffort") as string | undefined;
    const nextEffort =
      nextEfforts.length === 0
        ? undefined
        : currentEffort && nextEfforts.includes(currentEffort)
          ? currentEffort
          : nextEfforts[0];
    form.setFieldValue("defaultEffort", nextEffort);
    if (selectedRoleId) {
      const merged = {
        ...form.getFieldsValue(true),
        defaultModel: nextModel,
        defaultEffort: nextEffort,
      } as RoleFormValues;
      setDraftsById((prev) => ({ ...prev, [selectedRoleId]: merged }));
    }
  }

  function handleAddRole() {
    if (isNodeOffline || !canQueryRoles || busy) return;
    setActionError(null);
    setActionNotice(null);
    // 已有的和还没保存的新建角色 id 都要避开，不然两个草稿会撞 id
    const existingIds = new Set([...serverRoles.map((role) => role.id), ...draftNewRoles.map((draftRole) => draftRole.id)]);
    const defaultSource = ROLE_SOURCES.find((item) => item.available)?.value ?? "codex_exec";
    const defaultSlot = allowedSlotsForSource(defaultSource)[0];
    const newId = generateRoleId(existingIds, defaultSource);
    const fresh: ModelRole = {
      id: newId,
      slot: defaultSlot,
      name: "",
      source: defaultSource,
      default: { model: "", effort: null },
      models: [{ model: "", efforts: [] }],
    };
    setDraftNewRoles((prev) => [...prev, fresh]);
    setSelectedRoleId(newId);
    const initialValues = roleToFormValues(fresh);
    setDraftsById((prev) => ({ ...prev, [newId]: initialValues }));
    form.setFieldsValue(initialValues);
    lastSyncedRef.current = { roleId: newId, sig: configSignature(fresh) };
  }

  function handleResetCurrent() {
    if (!selectedRoleId) return;
    setActionError(null);
    setActionNotice(null);
    if (isEditingNewDraft) {
      setDraftNewRoles((prev) => prev.filter((draftRole) => draftRole.id !== selectedRoleId));
      setDraftsById((prev) => {
        const next = { ...prev };
        delete next[selectedRoleId];
        return next;
      });
      setSelectedRoleId(serverRoles[0]?.id ?? null);
      return;
    }
    setDraftsById((prev) => {
      const next = { ...prev };
      delete next[selectedRoleId];
      return next;
    });
    if (activeServerRole) {
      form.setFieldsValue(roleToFormValues(activeServerRole));
      lastSyncedRef.current = { roleId: selectedRoleId, sig: configSignature(activeServerRole) };
    }
  }

  // ---------------------------------------------------------------- 统一写操作（带审核口令流程）

  async function executeWrite(intent: WriteIntent, enteredToken?: string) {
    if (submitLock.current || (pendingWrite && enteredToken === undefined)) return;
    const token = enteredToken ?? readReviewToken();
    const ready = Boolean(selectedNodeId && canAct && !busy);
    const step = writeTokenAction(ready, token);
    if (step === "blocked") {
      setPendingWrite(null);
      return;
    }
    setActionError(null);
    if (step === "prompt") {
      setPendingWrite(intent);
      return;
    }
    if (enteredToken !== undefined) rememberReviewToken(enteredToken);
    submitLock.current = true;
    setPendingWrite(null);
    setBusyAction(intent.kind);
    setActionNotice(null);
    try {
      if (intent.kind === "save") {
        await saveNodeRoles(selectedNodeId, intent.payload, token, trimmedName);
        setDraftsById({});
        form.setFieldValue("apiKey", "");
        // 刚保存的新角色先留在列表里，等服务端轮询返回后再由上面的 effect 去掉本地草稿
        const savedCreated = intent.payload.filter((item) => intent.createdIds.includes(item.id)).map(toRoleInput);
        setDraftNewRoles(savedCreated);
        if (intent.createdIds.length > 0) setSelectedRoleId(intent.createdIds[0]);
        setActionNotice("角色配置已保存并下发到节点。");
      } else if (intent.kind === "release") {
        await releaseNode(selectedNodeId, token, trimmedName);
        setActionNotice("已解除绑定：这个节点现在没有主人，创建名字时可以认领它。节点若仍用原来的 NODE_USER_NAME 启动，重启后会再绑回去。");
      } else if (intent.kind === "init") {
        await initNodeRoles(selectedNodeId, token, trimmedName);
        setActionNotice("已用平台模板初始化角色并下发到节点，请逐个检测后再使用。");
      } else if (intent.kind === "check") {
        await checkNodeRole(selectedNodeId, intent.roleId, token, trimmedName);
        setActionNotice(`已向节点发起「${intent.roleName}」检测，结果将通过轮询自动更新。`);
      } else {
        await saveNodeRoles(selectedNodeId, intent.payload, token, trimmedName);
        setDraftsById((prev) => {
          const next = { ...prev };
          delete next[intent.roleId];
          return next;
        });
        setActionNotice(`角色「${intent.roleName}」已删除。`);
      }
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      submitLock.current = false;
      setBusyAction(null);
      reloadRoles();
    }
  }

  async function handleSave() {
    if (!selectedRoleId || isNodeOffline || busy) return;
    setActionError(null);
    setActionNotice(null);
    let activeValues: RoleFormValues;
    try {
      activeValues = await form.validateFields();
    } catch {
      return;
    }

    const activeValidationError = validateFormDraft(activeValues);
    if (activeValidationError) {
      setActionError(activeValidationError);
      return;
    }

    const mergedDrafts: Record<string, RoleFormValues> = {
      ...draftsById,
      [selectedRoleId]: activeValues,
    };

    // 校验其他尚未保存的草稿，防止提交不完整配置
    for (const [roleId, draftValues] of Object.entries(mergedDrafts)) {
      if (roleId === selectedRoleId) continue;
      const err = validateFormDraft(draftValues);
      if (err) {
        const label = draftValues.name?.trim() || roleId;
        setActionError(`角色「${label}」存在未完成配置：${err}`);
        return;
      }
    }

    // 整体替换：提交当前所有角色；只有被编辑过 Key 的角色才带 api_key
    const payload: RoleInput[] = serverRoles.map((role) => {
      const draft = mergedDrafts[role.id];
      if (!draft) return toRoleInput(role);
      return buildRoleFromForm(role.id, draft);
    });

    // 所有还没保存的新建角色一起提交
    const createdIds: string[] = [];
    for (const draftRole of draftNewRoles) {
      if (serverRoles.some((role) => role.id === draftRole.id)) continue;
      const newDraftValues = mergedDrafts[draftRole.id];
      if (!newDraftValues) {
        setActionError("请先完善新增角色的配置");
        return;
      }
      const input = buildRoleFromForm(draftRole.id, newDraftValues);
      payload.push(input);
      createdIds.push(input.id);
    }

    await executeWrite({ kind: "save", payload, createdIds });
  }

  /** 从「选应用」面板创建角色：和节点上现有的角色一起整体提交（走审核口令流程），新角色是第一个选中的 */
  function handleCreateFromApp(created: RoleInput[]) {
    if (busy || created.length === 0) return;
    setActionError(null);
    setActionNotice(null);
    const payload: RoleInput[] = [...serverRoles.map(toRoleInput), ...created];
    void executeWrite({ kind: "save", payload, createdIds: created.map((item) => item.id) });
  }

  function handleCheckCurrent() {
    if (!activeServerRole || isEditingNewDraft || isNodeOffline || busy) return;
    setActionError(null);
    setActionNotice(null);
    void executeWrite({
      kind: "check",
      roleId: activeServerRole.id,
      roleName: activeServerRole.name,
    });
  }

  function handleOpenDeleteConfirm() {
    if (!selectedRoleId || !activeRoleForForm || busy) return;
    setActionError(null);
    setDeleteTarget({
      id: selectedRoleId,
      name: (form.getFieldValue("name") as string | undefined)?.trim() || activeRoleForForm.name || "未命名角色",
      isDraftNew: isEditingNewDraft,
    });
  }

  function handleConfirmDelete() {
    if (!deleteTarget) return;
    const target = deleteTarget;
    setDeleteTarget(null);
    if (target.isDraftNew) {
      setDraftNewRoles((prev) => prev.filter((draftRole) => draftRole.id !== target.id));
      setDraftsById((prev) => {
        const next = { ...prev };
        delete next[target.id];
        return next;
      });
      setSelectedRoleId(serverRoles[0]?.id ?? null);
      setActionNotice("已放弃未保存的新建角色。");
      return;
    }
    // 整体替换提交剩余角色（不包含被删角色，且不附带 api_key）
    const remaining: RoleInput[] = serverRoles
      .filter((role) => role.id !== target.id)
      .map((role) => toRoleInput(role));
    void executeWrite({
      kind: "delete",
      roleId: target.id,
      roleName: target.name,
      payload: remaining,
    });
  }

  // ---------------------------------------------------------------- 顶部运行态文案

  const heroMode = isNodeOffline
    ? "offline"
    : !canQueryRoles
      ? "idle"
      : rolesError
        ? "offline"
        : "online";

  const runtimeStatus: { tone: StatusTone; text: string } = isNodeOffline
    ? { tone: "unc", text: "节点离线" }
    : !selectedNodeId
      ? { tone: "que", text: onlineNodes.length > 0 ? "请选择节点" : "暂无在线节点" }
      : rolesError
          ? { tone: "fail", text: "读取异常" }
          : !loadedForNode
            ? { tone: "run", text: "正在同步" }
            : stats.abnormal > 0
              ? { tone: "fail", text: "存在异常角色" }
              : stats.checking > 0
                ? { tone: "run", text: "检测进行中" }
                : { tone: "pass", text: "在线同步中" };

  const pipelineReady = stats.executorsOk > 0 && stats.judgesOk > 0;
  const activeRoleIsDirty = Boolean(selectedRoleId && draftsById[selectedRoleId]);

  return (
    <div className="roles-page">
      {/* 顶部通栏指挥带 */}
      <header
        className={`roles-hero${heroMode === "offline" ? " roles-hero-offline" : heroMode === "idle" ? " roles-hero-idle" : ""}`}
      >
        <div className="roles-hero-top">
          <div className="roles-hero-brand">
            <div className="roles-emblem" aria-hidden="true">&gt;_</div>
            <div className="roles-hero-titles">
              <div className="roles-hero-heading-row">
                <h1 className="roles-hero-title">角色与模型指挥台</h1>
                <span className="roles-hero-scope-tag">节点独立配置</span>
              </div>
              <div className="roles-hero-runtime">
                <span className={`roles-status-badge roles-tone-${runtimeStatus.tone}`}>
                  <span
                    className={`roles-status-dot${runtimeStatus.tone === "run" ? " roles-checking-dot" : ""}`}
                    aria-hidden="true"
                  />
                  {runtimeStatus.text}
                </span>
                {selectedNodeRecord && (
                  <>
                    <span className="roles-hero-sep" aria-hidden="true">·</span>
                    <span className="roles-mono" title={selectedNodeRecord.node_id}>
                      节点 {selectedNodeRecord.node_id.length > 12 ? `${selectedNodeRecord.node_id.slice(0, 12)}…` : selectedNodeRecord.node_id}
                    </span>
                    <span className="roles-hero-sep" aria-hidden="true">·</span>
                    <span>{selectedNodeRecord.caps.os}</span>
                    <span className="roles-hero-sep" aria-hidden="true">·</span>
                    <span className="roles-mono">{selectedNodeRecord.devices.length} 台设备</span>
                  </>
                )}
              </div>
              <div className="roles-hero-subnote">
                数据大家都能看，只有节点主人能改、能检测（用的是他自己的额度）；API Key 不回显、不落本地存储。
              </div>
            </div>
          </div>

          {/* 中央超大 KPI */}
          <div className="roles-hero-kpi" aria-label="可用角色概览">
            <div className="roles-kpi-main-row">
              <span className="roles-kpi-prefix">可用</span>
              <span className="roles-kpi-big">{loadedForNode && !isNodeOffline ? stats.ok : "—"}</span>
              <span className="roles-kpi-denom">
                / {loadedForNode && !isNodeOffline ? stats.total : "—"}
              </span>
            </div>
            <div className="roles-kpi-caption">
              {loadedForNode && !isNodeOffline
                ? `执行员 ${stats.executorsOk}/${stats.executors} 就绪 · 判定员 ${stats.judgesOk}/${stats.judges} 就绪`
                : "选择在线节点后显示"}
            </div>
          </div>

          {/* 右侧副 KPI */}
          <div className="roles-hero-side-kpis" aria-label="角色数量分布">
            <div className="roles-side-kpi-row">
              <span className="roles-side-kpi-label">执行员角色</span>
              <span className="roles-side-kpi-val">
                {loadedForNode && !isNodeOffline ? stats.executors : "—"}
              </span>
            </div>
            <div className="roles-side-kpi-row">
              <span className="roles-side-kpi-label">判定员角色</span>
              <span className={`roles-side-kpi-val${loadedForNode && !isNodeOffline && stats.judgesOk > 0 ? " is-pass" : ""}`}>
                {loadedForNode && !isNodeOffline ? stats.judges : "—"}
              </span>
            </div>
            <div className="roles-side-kpi-row">
              <span className="roles-side-kpi-label">异常角色</span>
              <span
                className={`roles-side-kpi-val${
                  !loadedForNode || isNodeOffline
                    ? " is-muted"
                    : stats.abnormal > 0
                      ? " is-fail"
                      : " is-muted"
                }`}
              >
                {loadedForNode && !isNodeOffline ? stats.abnormal : "—"}
              </span>
            </div>
          </div>
        </div>

        {/* 节点选择 + 我是谁 */}
        <div className="roles-hero-controls">
          <div className="roles-control-field">
            <label className="roles-control-label" htmlFor="roles-node-select">
              <span>在线节点</span>
              <span className="roles-control-hint">
                {nodes ? `共 ${onlineNodes.length} 个在线` : "正在加载节点…"}
              </span>
            </label>
            <Select<string>
              id="roles-node-select"
              aria-label="选择在线节点"
              value={selectedNodeId || undefined}
              onChange={(value) => setSelectedNodeId(value)}
              disabled={!nodes || onlineNodes.length === 0 || busy}
              placeholder={nodes && onlineNodes.length === 0 ? "当前没有在线节点" : "选择你的在线节点"}
              options={onlineNodes.map((candidate) => ({
                value: candidate.node_id,
                label: `${candidate.node_id} · ${candidate.caps.os} · ${candidate.devices.length} 台设备`,
              }))}
            />
          </div>

          <div className="roles-control-field">
            <label className="roles-control-label" htmlFor="roles-user-name">
              <span>我是谁</span>
              <IdentityHint className="roles-control-hint" userName={userName} expiresAt={nameExpiresAt}
                nameProblem={actionError === NEED_LOGIN_MESSAGE ? actionError : viewer?.name_problem}
                onForget={forgetName} onCreated={setUserName} onLogin={() => { setActionError(null); reloadRoles(); }} />
            </label>
            <Input
              id="roles-user-name"
              aria-label="我是谁"
              autoComplete="off"
              allowClear
              maxLength={40}
              disabled={busy}
              value={userName}
              onChange={(event) => {
                setUserName(event.target.value);
                if (actionError === NEED_LOGIN_MESSAGE) setActionError(null);
              }}
              placeholder="你的名字（不填也能看，只是不能改）"
            />
          </div>

          <div className="roles-control-meta">
            <span>自动轮询 {REFRESH_MS / 1000}s</span>
            {canAct && (
              <Button size="small" danger disabled={busy} loading={busyAction === "release"} onClick={() => void executeWrite({ kind: "release" })}>
                解除节点绑定
              </Button>
            )}
            {selectedNodeId && trimmedName && loadedForNode && rolesPoll.payload.unowned && (
              <Button size="small" type="primary" loading={claimBusy} onClick={() => void claimThisNode()}>
                认领这个节点
              </Button>
            )}
            {selectedNodeId && !canAct && (
              <Button size="small" danger disabled={busy} title="不是节点主人也能解除，要用管理员账号" onClick={() => setAdminOpen(true)}>
                管理员强制解除
              </Button>
            )}
            {dirtyCount > 0 && (
              <span className="roles-dirty-pill">{dirtyCount} 项未保存</span>
            )}
          </div>
        </div>
      </header>

      {/* 错误与状态提示区域 */}
      {(nodesError || rolesError || isNodeOffline || actionError || actionNotice) && (
        <div className="roles-alerts-stack">
          {nodesError && (
            <LoadError title="无法加载节点列表" error={nodesError} interval={REFRESH_MS} />
          )}
          {isNodeOffline && (
            <Alert
              type="warning"
              showIcon
              title="节点离线，无法读取和修改角色"
              description="请确认该执行节点已启动并重新连上平台，或在顶部切换到其他在线节点。"
            />
          )}
          {rolesError && !isNodeOffline && (
            <LoadError title="无法读取节点角色配置" error={rolesError} interval={REFRESH_MS} />
          )}
          {actionError && (
            <Alert
              type="error"
              showIcon
              closable
              onClose={() => setActionError(null)}
              title="操作未能完成"
              description={actionError}
            />
          )}
          {actionNotice && (
            <Alert
              type="info"
              showIcon
              closable
              onClose={() => setActionNotice(null)}
              title={actionNotice}
            />
          )}
        </div>
      )}

      {/* 选应用 → 检查本机环境 → 选模型创建角色。一进页面就显示，所有人都能看；只有节点主人能点（只读时说明原因） */}
      <AppsPanel
        nodeId={selectedNodeId}
        userName={trimmedName}
        online={!isNodeOffline}
        existingRoles={serverRoles}
        busy={busy}
        onCreateRoles={handleCreateFromApp}
        readOnlyReason={readOnlyReason}
      />

      {/* 门禁态：未选节点 / 节点离线 / 首次加载中 */}
      {!selectedNodeId ? (
        <section className="roles-gate-card" aria-label="等待选择在线节点">
          <Empty
            description={
              nodes && onlineNodes.length === 0
                ? "当前没有在线节点，请先启动自己的执行节点"
                : "请先在顶部选择一个在线节点"
            }
          />
        </section>
      ) : isNodeOffline ? (
        <section className="roles-gate-card" aria-label="节点离线">
          <h2 className="roles-gate-title">节点离线，无法读取和修改角色</h2>
          <p className="roles-gate-desc">
            节点恢复在线后会自动同步角色配置，你也可以在顶部选择其他在线节点。
          </p>
        </section>
      ) : isInitialLoading ? (
        <section className="roles-panel" aria-label="正在加载角色配置">
          <div className="roles-panel-body">
            <Skeleton active paragraph={{ rows: 7 }} />
          </div>
        </section>
      ) : rolesError && !loadedForNode ? null : (
        /* 主体左右两栏：左 28% 角色实体卡 / 右 72% 管线 + 统计 + 编辑 + 检测日志 */
        <div className="roles-workspace">
          {/* 左栏：角色实体卡列表 */}
          <aside className="roles-sidebar" aria-label="角色列表">
            <div className="roles-sidebar-head">
              <span className="roles-section-kicker">角色实体</span>
              <span className="roles-sidebar-count">{allDisplayRoles.length} 个角色</span>
            </div>

            {allDisplayRoles.length === 0 ? (
              <div className="roles-empty-sidebar">
                该节点还没有配置角色，可以用平台模板一键初始化（不含 Key，之后可修改），或点击下方「新增角色」自己添加。
                <Button
                  block
                  disabled={busy || isNodeOffline || readOnly}
                  loading={busyAction === "init"}
                  onClick={() => void executeWrite({ kind: "init" })}
                >
                  用平台模板初始化
                </Button>
              </div>
            ) : (
              <div className="roles-card-list">
                {allDisplayRoles.map((role) => {
                  const isSelected = role.id === selectedRoleId;
                  const isDirty = Boolean(draftsById[role.id]) || draftNewRoles.some((draftRole) => draftRole.id === role.id);
                  const meta = CHECK_META[role.check.status];
                  return (
                    <button
                      key={role.id}
                      type="button"
                      aria-pressed={isSelected}
                      onClick={() => {
                        setSelectedRoleId(role.id);
                        setActionError(null);
                      }}
                      className={`roles-entity-card${isSelected ? " is-selected" : ""}`}
                    >
                      <div className="roles-card-head">
                        <div className="roles-card-identity">
                          <span className={`roles-card-icon is-${role.slot}`}>
                            <SlotIcon slot={role.slot} />
                          </span>
                          <div className="roles-card-name-wrap">
                            <span className="roles-card-name" title={role.name}>
                              {role.name}
                            </span>
                            <div className="roles-card-tags">
                              <span className={`roles-slot-pill roles-slot-${role.slot}`}>
                                {SLOT_LABEL[role.slot]}
                              </span>
                              {role.key_configured && (
                                <span className="roles-key-pill">
                                  <KeyIcon />
                                  已配置 Key
                                </span>
                              )}
                              {isDirty && (
                                <span className="roles-dirty-pill">
                                  {draftNewRoles.some((draftRole) => draftRole.id === role.id) ? "新建未保存" : "未保存"}
                                </span>
                              )}
                            </div>
                          </div>
                        </div>
                        <StatusBadge status={role.check.status} />
                      </div>

                      <div className="roles-card-source" title={sourceLabel(role.source)}>
                        {sourceLabel(role.source)}
                      </div>

                      <div className="roles-card-model-box">
                        <span className="roles-card-model-name" title={role.default.model || "未设默认模型"}>
                          {role.default.model || "未设默认模型"}
                        </span>
                        {role.default.effort && (
                          <span className="roles-card-effort">{role.default.effort}</span>
                        )}
                      </div>

                      <div className="roles-card-foot">
                        <span className="roles-mono">{role.models.length} 个模型</span>
                        <span className="roles-mono">
                          {role.check.checked_at ? formatDate(role.check.checked_at) : "尚未检测"}
                        </span>
                      </div>

                      <div className="roles-card-meter" aria-hidden="true">
                        <div className={`roles-card-meter-fill roles-meter-${meta.tone}`} />
                      </div>
                    </button>
                  );
                })}
              </div>
            )}

            <button
              type="button"
              className="roles-add-btn"
              disabled={busy || isNodeOffline || readOnly}
              onClick={handleAddRole}
            >
              <span aria-hidden="true">+</span>
              <span>新增角色</span>
            </button>
          </aside>

          {/* 右栏：流程管线 + 统计块 + 编辑表单 + 类终端检测日志 */}
          <main className="roles-main">
            {/* 右栏上方：流程管线与 4 个统计块 */}
            <section className="roles-panel" aria-label="角色管线与状态统计">
              <div className="roles-panel-head">
                <div className="roles-panel-title-wrap">
                  <h2 className="roles-panel-title">执行管线 · 执行员 → 判定员</h2>
                  <span className="roles-panel-sub">按岗位分组展示各角色当前连通状态</span>
                </div>
                <span
                  className={`roles-status-badge ${
                    pipelineReady
                      ? "roles-tone-pass"
                      : stats.abnormal > 0
                        ? "roles-tone-fail"
                        : "roles-tone-que"
                  }`}
                >
                  <span className="roles-status-dot" aria-hidden="true" />
                  {pipelineReady ? "双岗均可用" : stats.abnormal > 0 ? "管线存在异常" : "待完成检测"}
                </span>
              </div>

              <div className="roles-flow-body">
                <div className="roles-pipeline" role="group" aria-label="执行员到判定员流程图">
                  {/* 执行员分组 */}
                  <div className="roles-pipe-stage">
                    <div className="roles-pipe-stage-head">
                      <span className="roles-slot-pill roles-slot-executor">执行员角色</span>
                      <span className="roles-pipe-stage-count">
                        可用 {stats.executorsOk} / {stats.executors}
                      </span>
                    </div>
                    {executorRoles.length === 0 ? (
                      <div className="roles-pipe-empty">暂未配置执行员角色</div>
                    ) : (
                      <div className="roles-pipe-nodes">
                        {executorRoles.map((role) => {
                          const meta = CHECK_META[role.check.status];
                          return (
                            <button
                              key={role.id}
                              type="button"
                              onClick={() => setSelectedRoleId(role.id)}
                              className={`roles-pipe-node${selectedRoleId === role.id ? " is-selected" : ""}`}
                            >
                              <div className="roles-pipe-node-top">
                                <span className="roles-pipe-node-name" title={role.name}>
                                  {role.name}
                                </span>
                                <span className={`roles-pipe-node-avail ${pipeAvailClass(role.check.status)}`}>
                                  {meta.available ? "可用" : `不可用 · ${meta.text}`}
                                </span>
                              </div>
                              <span className="roles-pipe-node-meta" title={role.default.model}>
                                {role.default.model || "未设默认模型"}
                                {role.default.effort ? ` · ${role.default.effort}` : ""}
                              </span>
                            </button>
                          );
                        })}
                      </div>
                    )}
                  </div>

                  {/* 流向箭头 */}
                  <div className="roles-pipe-bridge" aria-hidden="true">
                    <div className={`roles-pipe-bridge-line${pipelineReady ? " is-ready" : ""}`}>
                      <svg width="48" height="16" viewBox="0 0 48 16" fill="none">
                        <path
                          d="M2 8h40"
                          stroke="currentColor"
                          strokeWidth="1.5"
                          strokeDasharray="4 3"
                          strokeLinecap="round"
                        />
                        <path
                          d="M38 3.5L44 8l-6 4.5"
                          stroke="currentColor"
                          strokeWidth="1.5"
                          strokeLinecap="round"
                          strokeLinejoin="round"
                        />
                      </svg>
                    </div>
                    <span className="roles-pipe-bridge-label">提交判定</span>
                  </div>

                  {/* 判定员分组 */}
                  <div className="roles-pipe-stage">
                    <div className="roles-pipe-stage-head">
                      <span className="roles-slot-pill roles-slot-judge">判定员角色</span>
                      <span className="roles-pipe-stage-count">
                        可用 {stats.judgesOk} / {stats.judges}
                      </span>
                    </div>
                    {judgeRoles.length === 0 ? (
                      <div className="roles-pipe-empty">暂未配置判定员角色</div>
                    ) : (
                      <div className="roles-pipe-nodes">
                        {judgeRoles.map((role) => {
                          const meta = CHECK_META[role.check.status];
                          return (
                            <button
                              key={role.id}
                              type="button"
                              onClick={() => setSelectedRoleId(role.id)}
                              className={`roles-pipe-node${selectedRoleId === role.id ? " is-selected" : ""}`}
                            >
                              <div className="roles-pipe-node-top">
                                <span className="roles-pipe-node-name" title={role.name}>
                                  {role.name}
                                </span>
                                <span className={`roles-pipe-node-avail ${pipeAvailClass(role.check.status)}`}>
                                  {meta.available ? "可用" : `不可用 · ${meta.text}`}
                                </span>
                              </div>
                              <span className="roles-pipe-node-meta" title={role.default.model}>
                                {role.default.model || "未设默认模型"}
                                {role.default.effort ? ` · ${role.default.effort}` : ""}
                              </span>
                            </button>
                          );
                        })}
                      </div>
                    )}
                  </div>
                </div>

                {/* 4 个统计块 */}
                <div className="roles-stats-grid" role="group" aria-label="角色统计指标">
                  <div className="roles-stat-tile">
                    <span className="roles-stat-label">角色总数</span>
                    <span className="roles-stat-num">{stats.total}</span>
                    <span className="roles-stat-note">
                      执行员 {stats.executors} · 判定员 {stats.judges}
                    </span>
                  </div>
                  <div className="roles-stat-tile">
                    <span className="roles-stat-label">可用</span>
                    <span className={`roles-stat-num${stats.ok > 0 ? " is-pass" : ""}`}>
                      {stats.ok}
                    </span>
                    <span className="roles-stat-note">最近一次检测通过</span>
                  </div>
                  <div className="roles-stat-tile">
                    <span className="roles-stat-label">未检测</span>
                    <span className="roles-stat-num is-que">{stats.unchecked}</span>
                    <span className="roles-stat-note">
                      {stats.checking > 0 ? `另有 ${stats.checking} 个检测中` : "尚未发起连通检测"}
                    </span>
                  </div>
                  <div className={`roles-stat-tile${stats.abnormal > 0 ? " is-alert" : ""}`}>
                    <span className="roles-stat-label">异常</span>
                    <span className={`roles-stat-num${stats.abnormal > 0 ? " is-fail" : ""}`}>
                      {stats.abnormal}
                    </span>
                    <span className="roles-stat-note">失败 / 未登录 / 额度用完</span>
                  </div>
                </div>
              </div>
            </section>

            {/* 右栏中部：选中角色编辑表单 */}
            <section className="roles-panel" aria-label="角色配置编辑">
              <div className="roles-panel-head">
                <div className="roles-panel-title-wrap">
                  <h2 className="roles-panel-title">
                    {!activeRoleForForm
                      ? "角色详情与配置"
                      : isEditingNewDraft
                        ? "新增角色"
                        : `编辑角色 · ${activeRoleForForm.name}`}
                  </h2>
                  {activeServerRole && <StatusBadge status={activeServerRole.check.status} />}
                  {activeRoleIsDirty && <span className="roles-dirty-pill">有未保存修改</span>}
                </div>

                {activeRoleForForm && (
                  <div className="roles-editor-actions">
                    {activeRoleIsDirty && (
                      <Button size="small" disabled={busy} onClick={handleResetCurrent}>
                        {isEditingNewDraft ? "放弃新建" : "还原"}
                      </Button>
                    )}
                    <Button
                      size="small"
                      disabled={busy || isEditingNewDraft || isNodeOffline || readOnly}
                      loading={busyAction === "check"}
                      onClick={handleCheckCurrent}
                      title={isEditingNewDraft ? "请先保存角色后再检测" : "让节点真调一次模型检测可用性"}
                    >
                      检测
                    </Button>
                    <Button
                      size="small"
                      danger
                      disabled={busy || isNodeOffline || readOnly}
                      loading={busyAction === "delete"}
                      onClick={handleOpenDeleteConfirm}
                    >
                      删除角色
                    </Button>
                    <Button
                      size="small"
                      type="primary"
                      disabled={busy || isNodeOffline || readOnly}
                      loading={busyAction === "save"}
                      onClick={() => void handleSave()}
                    >
                      保存
                    </Button>
                  </div>
                )}
              </div>

              <div className="roles-panel-body">
                {!activeRoleForForm ? (
                  <Empty description="请在左侧选择一个角色，或点击「新增角色」创建配置" />
                ) : (
                  <>
                    <div className="roles-editor-meta-bar">
                      <div className="roles-editor-meta-items">
                        <span>
                          角色标识：<code className="roles-mono">{activeRoleForForm.id}</code>
                        </span>
                        {activeServerRole?.key_configured && (
                          <span className="roles-key-pill">
                            <KeyIcon />
                            已配置 Key
                          </span>
                        )}
                      </div>
                      <span className="roles-mono">
                        {activeServerRole?.check.checked_at
                          ? `上次检测：${formatDate(activeServerRole.check.checked_at)}`
                          : isEditingNewDraft
                            ? "保存后可发起检测"
                            : "尚未检测"}
                      </span>
                    </div>

                    <Form<RoleFormValues>
                      form={form}
                      layout="vertical"
                      disabled={busy || isNodeOffline || readOnly}
                      onValuesChange={handleFormValuesChange}
                    >
                      <div className="roles-form-grid">
                        <Form.Item
                          name="name"
                          label="角色名称"
                          rules={[{ required: true, whitespace: true, message: "请输入角色名称" }]}
                        >
                          <Input placeholder="例如：Codex 执行员 / Claude 判定员" maxLength={60} />
                        </Form.Item>

                        <Form.Item
                          name="slot"
                          label="岗位"
                          rules={[{ required: true, message: "请选择岗位" }]}
                          extra={
                            allowedSlots.length === 1
                              ? `当前来源仅支持担任${SLOT_LABEL[allowedSlots[0]]}`
                              : "选择该角色在测试流程中的岗位"
                          }
                        >
                          <Segmented<ModelRole["slot"]>
                            block
                            options={[
                              {
                                value: "executor",
                                label: "执行员",
                                disabled: !allowedSlots.includes("executor"),
                              },
                              {
                                value: "judge",
                                label: "判定员",
                                disabled: !allowedSlots.includes("judge"),
                              },
                              {
                                value: "case_writer",
                                label: "测试用例",
                                disabled: !allowedSlots.includes("case_writer"),
                              },
                              {
                                value: "planner",
                                label: "管理员",
                                disabled: !allowedSlots.includes("planner"),
                              },
                            ]}
                          />
                        </Form.Item>

                        <Form.Item
                          name="source"
                          label="模型来源"
                          className="roles-form-span-2"
                          rules={[{ required: true, message: "请选择模型来源" }]}
                          extra={selectedSourceMeta?.hint}
                        >
                          <Select<string, SourceOption>
                            onChange={handleSourceChange}
                            optionLabelProp="label"
                            options={ROLE_SOURCES.map((item) => ({
                              value: item.value,
                              label: item.available ? item.label : `${item.label}（${item.hint}）`,
                              disabled: !item.available,
                              hint: item.hint,
                              rawLabel: item.label,
                            }))}
                            optionRender={(option) => (
                              <div className="roles-source-option">
                                <div className="roles-source-option-top">
                                  <span>{option.data.rawLabel}</span>
                                  {option.data.disabled && (
                                    <span className="roles-status-badge roles-tone-que">暂不可用</span>
                                  )}
                                </div>
                                <span className="roles-source-option-hint">
                                  {option.data.hint}
                                </span>
                              </div>
                            )}
                          />
                        </Form.Item>
                      </div>

                      {/* 模型列表 */}
                      <Form.Item label="模型列表" required>
                        <Form.List name="models">
                          {(fields, { add, remove }) => (
                            <div className="roles-models-box">
                              <div className="roles-models-header" aria-hidden="true">
                                <span>模型名称</span>
                                <span>推理强度档位（可留空，回车添加多个标签）</span>
                                <span style={{ textAlign: "right" }}>操作</span>
                              </div>

                              {fields.map((field) => (
                                <div key={field.key} className="roles-model-row">
                                  <Form.Item
                                    name={[field.name, "model"]}
                                    rules={[{ required: true, whitespace: true, message: "请输入模型名" }]}
                                  >
                                    <Input
                                      aria-label="模型名称"
                                      placeholder="例如：gpt-5.4 或 claude-opus-4-6"
                                    />
                                  </Form.Item>

                                  <Form.Item name={[field.name, "efforts"]}>
                                    <Select<string[]>
                                      mode="tags"
                                      aria-label="推理强度档位"
                                      tokenSeparators={[",", "，", " "]}
                                      open={false}
                                      suffixIcon={null}
                                      placeholder="留空表示无单独档位；可输入如 low / medium / high"
                                    />
                                  </Form.Item>

                                  <div className="roles-model-remove">
                                    <Button
                                      size="small"
                                      danger
                                      disabled={fields.length <= 1 || busy || isNodeOffline}
                                      onClick={() => {
                                        const rows = (form.getFieldValue("models") as ModelRowField[] | undefined) ?? [];
                                        const removedName = (rows[field.name]?.model ?? "").trim();
                                        remove(field.name);
                                        const afterRows = rows.filter((_, idx) => idx !== field.name);
                                        if (removedName && removedName === watchedDefaultModel.trim()) {
                                          const fallback = afterRows.find((r) => r?.model?.trim());
                                          const fallbackName = fallback?.model.trim() ?? "";
                                          const fallbackEffort = fallback?.efforts?.[0];
                                          form.setFieldsValue({
                                            defaultModel: fallbackName,
                                            defaultEffort: fallbackEffort,
                                          });
                                        }
                                        if (selectedRoleId) {
                                          const snapshot = form.getFieldsValue(true) as RoleFormValues;
                                          setDraftsById((prev) => ({ ...prev, [selectedRoleId]: snapshot }));
                                        }
                                      }}
                                    >
                                      移除
                                    </Button>
                                  </div>
                                </div>
                              ))}

                              <div className="roles-models-foot">
                                <Button
                                  size="small"
                                  disabled={busy || isNodeOffline || readOnly}
                                  onClick={() => add({ model: "", efforts: [] })}
                                >
                                  + 添加模型
                                </Button>
                                <span className="roles-models-tip">
                                  推理强度已写进模型名的来源（如 agy）可直接留空档位。
                                </span>
                              </div>
                            </div>
                          )}
                        </Form.List>
                      </Form.Item>

                      {/* 默认模型与默认推理强度 */}
                      <div className="roles-form-grid">
                        <Form.Item
                          name="defaultModel"
                          label="默认模型"
                          rules={[{ required: true, message: "请选择默认模型" }]}
                        >
                          <Select<string>
                            placeholder={
                              availableModelNames.length > 0
                                ? "从上方模型列表中选择默认模型"
                                : "请先在上方填写模型名称"
                            }
                            onChange={handleDefaultModelChange}
                            options={availableModelNames.map((name) => ({
                              value: name,
                              label: name,
                            }))}
                          />
                        </Form.Item>

                        <Form.Item
                          name="defaultEffort"
                          label="默认推理强度"
                          extra={
                            defaultModelEfforts.length === 0
                              ? "当前默认模型未配置单独推理强度档位"
                              : undefined
                          }
                        >
                          <Select<string>
                            allowClear
                            disabled={defaultModelEfforts.length === 0 || busy || isNodeOffline}
                            placeholder={
                              defaultModelEfforts.length > 0 ? "选择默认推理强度" : "无单独强度档位"
                            }
                            options={defaultModelEfforts.map((effort) => ({
                              value: effort,
                              label: effort,
                            }))}
                          />
                        </Form.Item>
                      </div>

                      {/* API Key：仅来源 needsKey 时显示，绝不回显 */}
                      {showApiKeyField && (
                        <div className="roles-key-section">
                          <Form.Item
                            name="apiKey"
                            label="API Key"
                            extra="Key 仅随本次保存发往你的节点写入本机，平台不落库、不回显。"
                          >
                            <Input.Password
                              autoComplete="new-password"
                              visibilityToggle={false}
                              placeholder={
                                activeServerRole?.key_configured
                                  ? "已配置，留空表示不更换"
                                  : "请输入 Key"
                              }
                            />
                          </Form.Item>
                        </div>
                      )}
                    </Form>
                  </>
                )}
              </div>
            </section>

            {/* 右栏下方：类终端检测日志面板 */}
            <section className="roles-terminal" aria-label="角色检测日志">
              <div className="roles-terminal-bar">
                <div className="roles-terminal-cmd">
                  <span>$ role-check.log</span>
                  <span className="roles-terminal-title">检测日志</span>
                </div>
                <span className="roles-terminal-count">
                  {checkLogEntries.length} 条记录
                </span>
              </div>

              <div className="roles-terminal-body">
                {checkLogEntries.length === 0 ? (
                  <div className="roles-terminal-empty">还没有检测记录</div>
                ) : (
                  checkLogEntries.map((role) => (
                    <div key={role.id} className="roles-log-entry">
                      <div className="roles-log-line">
                        <span className="roles-log-time">
                          {role.check.checked_at ? formatDate(role.check.checked_at) : "检测进行中"}
                        </span>
                        <span className="roles-log-role">{role.name}</span>
                        <span className={`roles-slot-pill roles-slot-${role.slot}`}>
                          {SLOT_LABEL[role.slot]}
                        </span>
                        <StatusBadge status={role.check.status} />
                      </div>
                      {role.check.detail && (
                        <pre className="roles-log-detail">{role.check.detail}</pre>
                      )}
                    </div>
                  ))
                )}
              </div>
            </section>
          </main>
        </div>
      )}

      {/* 管理员强制解除节点绑定 */}
      <Modal
        open={adminOpen}
        title="管理员强制解除节点绑定"
        okText="解除"
        cancelText="取消"
        okButtonProps={{ danger: true, loading: adminBusy, disabled: !adminName.trim() || !adminPassword }}
        onOk={() => void forceRelease()}
        onCancel={() => setAdminOpen(false)}
        destroyOnHidden
      >
        <div style={{ display: "grid", gap: 12 }}>
          <Input aria-label="管理员账号" placeholder="管理员账号" value={adminName} onChange={(e) => setAdminName(e.target.value)} autoComplete="off" />
          <Input.Password aria-label="管理员密码" placeholder="管理员密码" value={adminPassword} onChange={(e) => setAdminPassword(e.target.value)} onPressEnter={() => void forceRelease()} autoComplete="new-password" />
        </div>
      </Modal>

      {/* 删除角色二次确认弹窗 */}
      {deleteTarget && (
        <Modal
          open
          title={`删除角色：${deleteTarget.name}`}
          okText="确认删除"
          cancelText="取消"
          okButtonProps={{ danger: true, loading: busyAction === "delete" }}
          cancelButtonProps={{ disabled: busy }}
          onOk={handleConfirmDelete}
          onCancel={() => setDeleteTarget(null)}
        >
          <p>
            {deleteTarget.isDraftNew
              ? "确定放弃这个尚未保存的新建角色吗？"
              : `确定从当前节点配置中删除「${deleteTarget.name}」吗？删除后会立即提交整体角色配置到节点。`}
          </p>
        </Modal>
      )}

      {/* 审核口令弹窗（保存 / 检测 / 删除复用 ReviewTokenDialog） */}
      {pendingWrite && (
        <ReviewTokenDialog
          disabled={busy}
          onConfirm={(token) => void executeWrite(pendingWrite, token)}
          onCancel={() => setPendingWrite(null)}
        />
      )}
    </div>
  );
}
