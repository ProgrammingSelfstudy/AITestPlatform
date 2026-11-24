import { Alert, AutoComplete, Button, Input, Modal, Select } from "antd";
import type { ChangeEvent, DragEvent } from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  createCasegenRequest,
  fetchModules,
  fetchNodeRoles,
  fetchNodes,
  ROLE_SOURCES,
  type CasegenInput,
  type CheckStatus,
  type ModuleView,
  type NodeRoles,
  type NodeView,
  type RoleView,
} from "./api";
import { readReviewToken, rememberReviewToken, writeTokenAction } from "./reviewLogic";
import ReviewTokenDialog from "./ReviewTokenDialog";
import { LoadError } from "./runUi";
import { REFRESH_MS, usePolling } from "./usePolling";
import IdentityHint from "./IdentityHint";
import { fetchFlowTemplates, type FlowTemplate } from "./api";
import { NEED_LOGIN_MESSAGE, useRememberedUserName } from "./identity";
import "./modules.css";

const project = "hafa";
const MAX_MODULE_LEN = 80;
const MAX_DOC_LEN = 200_000;
const MAX_LINKS = 5;
const MAX_SCREENSHOTS = 100;
const MAX_IMAGE_BYTES = 3 * 1024 * 1024;
/** 全部截图合计的上限（和服务端一致） */
const MAX_TOTAL_IMAGE_BYTES = 100 * 1024 * 1024;
const ALLOWED_IMAGE_TYPES = new Set(["image/png", "image/jpeg"]);

type Tone = "pass" | "run" | "que" | "unc" | "lost" | "fail";

const CHECK_STATUS_META: Record<CheckStatus, { text: string; tone: Tone; ok: boolean }> = {
  unchecked: { text: "未检测", tone: "que", ok: false },
  checking: { text: "检测中", tone: "run", ok: false },
  ok: { text: "可用", tone: "pass", ok: true },
  not_logged_in: { text: "未登录", tone: "unc", ok: false },
  quota_exhausted: { text: "额度用完", tone: "lost", ok: false },
  failed: { text: "检测失败", tone: "fail", ok: false },
};

interface ScreenshotItem {
  id: string;
  name: string;
  size: number;
  previewUrl: string;
  data_b64: string;
}

interface RoleSelectOption {
  value: string;
  label: string;
  disabled: boolean;
  role: RoleView;
}

interface ValidationErrors {
  module?: string;
  doc?: string;
  links?: string;
  screenshots?: string;
  node?: string;
  userName?: string;
  writerRole?: string;
  reviewerRole?: string;
}

function sourceLabel(source: string): string {
  return ROLE_SOURCES.find((item) => item.value === source)?.label ?? source;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
}

function readFileAsText(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      resolve(typeof reader.result === "string" ? reader.result : "");
    };
    reader.onerror = () => reject(new Error("读取文档文件失败，请重试"));
    reader.readAsText(file, "utf-8");
  });
}

function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      if (typeof reader.result === "string") {
        resolve(reader.result);
      } else {
        reject(new Error("读取图片失败"));
      }
    };
    reader.onerror = () => reject(new Error("读取图片失败"));
    reader.readAsDataURL(file);
  });
}

function stripDataUrlPrefix(dataUrl: string): string {
  const idx = dataUrl.indexOf("base64,");
  return idx >= 0 ? dataUrl.slice(idx + "base64,".length) : dataUrl;
}

function RequiredLabel({ text }: { text: string }) {
  return (
    <span className="mod-field-label">
      <span>{text}</span>
      <span className="mod-req-tag">必填</span>
    </span>
  );
}

function UploadIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 20 20" fill="none" aria-hidden="true">
      <path
        d="M10 13V4m0 0L6.5 7.5M10 4l3.5 3.5M4 13.5v1.25A1.25 1.25 0 005.25 16h9.5A1.25 1.25 0 0016 14.75V13.5"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function RoleStatusBadge({ status }: { status: CheckStatus }) {
  const meta = CHECK_STATUS_META[status];
  return (
    <span className={`mod-badge mod-tone-${meta.tone}`}>
      <span className={`mod-badge-dot${status === "checking" ? " mod-pulse-dot" : ""}`} aria-hidden="true" />
      {meta.text}
    </span>
  );
}

const onceLoaded = () => false;

export default function GenerateDialog({ onClose, initialModule }: { onClose: () => void; initialModule?: string }) {
  // 模块选项（加载一次）
  const loadModulesOnce = useCallback((signal: AbortSignal) => fetchModules(project, signal), []);
  const { data: existingModules } = usePolling<ModuleView[]>(loadModulesOnce, REFRESH_MS, onceLoaded);

  // 节点列表轮询（防闪烁）
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

  // 表单状态（「我是谁」只记名字，在本浏览器记 30 天）
  // 从管理员的提议进来时，模块名预填好；其余（链接、截图）仍要人自己补
  const [moduleName, setModuleName] = useState(initialModule ?? "");
  const [doc, setDoc] = useState("");
  const [docUploadNote, setDocUploadNote] = useState<string | null>(null);
  const [links, setLinks] = useState<string[]>([""]);
  const [screenshots, setScreenshots] = useState<ScreenshotItem[]>([]);
  const [screenshotNotice, setScreenshotNotice] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState(false);

  const [nodeId, setNodeId] = useState("");
  const { name: userName, setName: setUserName, expiresAt: nameExpiresAt, forget: forgetName } = useRememberedUserName();
  const trimmedName = userName.trim();

  // 节点角色拉取（节点 + 名字都填完后拉一次，也支持手动刷新）
  const [rolesState, setRolesState] = useState<{
    loading: boolean;
    error: string | null;
    data: NodeRoles | null;
    forKey: string;
  }>({ loading: false, error: null, data: null, forKey: "" });
  const [rolesRefreshSeq, setRolesRefreshSeq] = useState(0);

  // 流程模板（参数配置里配的，例如「研发测试流程」）：选了流程，就按里面的角色名在节点上找对应岗位的角色
  const [flows, setFlows] = useState<FlowTemplate[]>([]);
  const [flowCode, setFlowCode] = useState("");
  const [flowNotes, setFlowNotes] = useState<string[]>([]);
  useEffect(() => {
    const controller = new AbortController();
    void fetchFlowTemplates(controller.signal).then((list) => {
      if (controller.signal.aborted) return;
      setFlows(list);
      setFlowCode((prev) => prev || list[0]?.code || "");
    });
    return () => controller.abort();
  }, []);
  const [writerRole, setWriterRole] = useState("");
  const [reviewerRole, setReviewerRole] = useState("");

  // 校验、提交与审核口令
  const [errors, setErrors] = useState<ValidationErrors>({});
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [pendingInput, setPendingInput] = useState<CasegenInput | null>(null);
  const submitLock = useRef(false);
  const docFileInputRef = useRef<HTMLInputElement | null>(null);
  const imgFileInputRef = useRef<HTMLInputElement | null>(null);
  const screenshotIdSeq = useRef(0);
  const screenshotsCountRef = useRef(0);
  screenshotsCountRef.current = screenshots.length;

  const busy = submitting || pendingInput !== null;

  // 默认选中第一个在线节点（不覆盖用户已选）
  useEffect(() => {
    if (!nodeId && onlineNodes.length > 0) {
      setNodeId(onlineNodes[0].node_id);
    }
  }, [nodeId, onlineNodes]);

  const selectedNodeRecord = useMemo(
    () => nodes?.find((candidate) => candidate.node_id === nodeId) ?? null,
    [nodes, nodeId],
  );

  const nodeRolesOffline = Boolean(
    (selectedNodeRecord && !selectedNodeRecord.online) ||
    (rolesState.data && rolesState.forKey === `${nodeId}\0${trimmedName}` && !rolesState.data.online),
  );

  // 节点与名字都就绪后，拉取一次节点角色（带轻微防抖，避免输入名字时每个字符都请求）
  useEffect(() => {
    if (!nodeId || !trimmedName) {
      setRolesState({ loading: false, error: null, data: null, forKey: "" });
      setWriterRole("");
      setReviewerRole("");
      return;
    }
    const key = `${nodeId}\0${trimmedName}`;
    const controller = new AbortController();
    const timer = window.setTimeout(() => {
      setRolesState((prev) => ({
        loading: true,
        error: null,
        data: prev.forKey === key ? prev.data : null,
        forKey: key,
      }));
      void fetchNodeRoles(nodeId, trimmedName, controller.signal)
        .then((payload) => {
          if (controller.signal.aborted) return;
          setRolesState({ loading: false, error: null, data: payload, forKey: key });
        })
        .catch((err: unknown) => {
          if (controller.signal.aborted) return;
          const message = err instanceof Error ? err.message : String(err);
          setRolesState({ loading: false, error: message, data: null, forKey: key });
        });
    }, 300);

    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [nodeId, trimmedName, rolesRefreshSeq]);

  const currentRoles: RoleView[] = useMemo(() => {
    const key = `${nodeId}\0${trimmedName}`;
    if (rolesState.forKey !== key || !rolesState.data || !rolesState.data.online) {
      return [];
    }
    return rolesState.data.roles;
  }, [nodeId, trimmedName, rolesState]);

  const rolesLoaded = Boolean(
    nodeId &&
    trimmedName &&
    rolesState.forKey === `${nodeId}\0${trimmedName}` &&
    !rolesState.loading &&
    rolesState.data !== null,
  );

  const writerRoles = useMemo(
    () => currentRoles.filter((role) => role.slot === "case_writer"),
    [currentRoles],
  );
  const judgeRoles = useMemo(
    () => currentRoles.filter((role) => role.slot === "judge"),
    [currentRoles],
  );

  const okWriterRoles = useMemo(
    () => writerRoles.filter((role) => role.check.status === "ok"),
    [writerRoles],
  );
  const okJudgeRoles = useMemo(
    () => judgeRoles.filter((role) => role.check.status === "ok"),
    [judgeRoles],
  );

  // 角色列表就绪时自动选择第一个可用角色；若原选择已不可用则清空
  useEffect(() => {
    if (!rolesLoaded || nodeRolesOffline) return;
    setWriterRole((prev) => {
      if (prev && okWriterRoles.some((r) => r.id === prev)) return prev;
      return okWriterRoles[0]?.id ?? "";
    });
    setReviewerRole((prev) => {
      if (prev && okJudgeRoles.some((r) => r.id === prev)) return prev;
      return okJudgeRoles[0]?.id ?? "";
    });
  }, [rolesLoaded, nodeRolesOffline, okWriterRoles, okJudgeRoles]);

  // 选了流程：用流程里的角色名给「测试用例」「评审员」岗位选角色；找不到或没通过检测的，如实写出来
  useEffect(() => {
    const flow = flows.find((f) => f.code === flowCode);
    if (!flow || !rolesLoaded || nodeRolesOffline) {
      setFlowNotes([]);
      return;
    }
    const notes: string[] = [];
    const pick = (slot: "case_writer" | "judge", label: string, roles: RoleView[]): string | null => {
      const wanted = flow.roles[slot];
      if (!wanted) return null;
      const found = roles.find((r) => r.name === wanted);
      if (!found) {
        notes.push(`流程里的${label}「${wanted}」在这个节点上不存在`);
        return null;
      }
      if (found.check.status !== "ok") {
        notes.push(`流程里的${label}「${wanted}」还没通过检测`);
        return null;
      }
      return found.id;
    };
    const writer = pick("case_writer", "测试用例角色", writerRoles);
    const reviewer = pick("judge", "判定员角色", judgeRoles);
    if (!flow.roles.case_writer) notes.push("这个流程没有指定测试用例角色，请手动选");
    if (!flow.roles.judge) notes.push("这个流程没有指定判定员角色，请手动选");
    if (writer) setWriterRole(writer);
    if (reviewer) setReviewerRole(reviewer);
    setFlowNotes(notes);
  }, [flows, flowCode, rolesLoaded, nodeRolesOffline, writerRoles, judgeRoles]);

  const selectedWriterRoleObj = useMemo(
    () => writerRoles.find((r) => r.id === writerRole) ?? null,
    [writerRoles, writerRole],
  );
  const selectedJudgeRoleObj = useMemo(
    () => judgeRoles.find((r) => r.id === reviewerRole) ?? null,
    [judgeRoles, reviewerRole],
  );

  // 模块 AutoComplete 选项
  const moduleOptions = useMemo(() => {
    const q = moduleName.trim().toLowerCase();
    return (existingModules ?? [])
      .filter((mod) => !q || mod.name.toLowerCase().includes(q))
      .map((mod) => ({
        value: mod.name,
        label: `${mod.name}（已有 ${mod.case_count} 条用例）`,
      }));
  }, [existingModules, moduleName]);

  // 处理文档上传（.md / .txt）
  async function handleDocFileChange(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    event.target.value = "";
    if (!file) return;
    const lower = file.name.toLowerCase();
    if (!lower.endsWith(".md") && !lower.endsWith(".txt")) {
      setDocUploadNote("仅支持上传 .md 或 .txt 文本文件");
      return;
    }
    try {
      const content = await readFileAsText(file);
      setDoc(content);
      setDocUploadNote(`已载入「${file.name}」（${content.length.toLocaleString("zh-CN")} 字）`);
      setErrors((prev) => ({ ...prev, doc: undefined }));
    } catch (err) {
      setDocUploadNote(err instanceof Error ? err.message : String(err));
    }
  }

  // 处理截图添加（点击、拖拽、粘贴剪贴板共用）
  const addScreenshotFiles = useCallback(async (incoming: File[]) => {
    if (incoming.length === 0) return;
    const messages: string[] = [];
    const validFiles: File[] = [];

    for (const file of incoming) {
      if (!ALLOWED_IMAGE_TYPES.has(file.type)) {
        messages.push(`「${file.name || "剪贴板图片"}」格式不支持，仅接受 PNG / JPEG`);
        continue;
      }
      if (file.size > MAX_IMAGE_BYTES) {
        messages.push(
          `「${file.name || "剪贴板图片"}」体积 ${formatBytes(file.size)}，超过单张 3MB 上限`,
        );
        continue;
      }
      validFiles.push(file);
    }

    if (validFiles.length === 0) {
      if (messages.length > 0) setScreenshotNotice(messages.join("；"));
      return;
    }

    const loaded: ScreenshotItem[] = [];
    for (const file of validFiles) {
      try {
        const previewUrl = await readFileAsDataUrl(file);
        screenshotIdSeq.current += 1;
        loaded.push({
          id: `shot_${screenshotIdSeq.current}`,
          name: file.name || `剪贴板截图-${screenshotIdSeq.current}.${file.type === "image/jpeg" ? "jpg" : "png"}`,
          size: file.size,
          previewUrl,
          data_b64: stripDataUrlPrefix(previewUrl),
        });
      } catch {
        messages.push(`「${file.name || "图片"}」读取失败`);
      }
    }

    if (loaded.length > 0) {
      const remaining = MAX_SCREENSHOTS - screenshotsCountRef.current;
      if (remaining <= 0) {
        messages.push(`最多上传 ${MAX_SCREENSHOTS} 张 UI 区域截图，请先删除已有图片`);
      } else {
        const toAdd = loaded.slice(0, remaining);
        if (loaded.length > remaining) {
          messages.push(`最多上传 ${MAX_SCREENSHOTS} 张 UI 区域截图，已保留前 ${remaining} 张并忽略超出部分`);
        }
        screenshotsCountRef.current += toAdd.length;
        setScreenshots((prev) => [...prev, ...toAdd].slice(0, MAX_SCREENSHOTS));
        setErrors((prev) => ({ ...prev, screenshots: undefined }));
      }
    }

    setScreenshotNotice(messages.length > 0 ? messages.join("；") : null);
  }, []);

  // 全局监听剪贴板粘贴图片（不拦截普通文本粘贴）
  useEffect(() => {
    function handleWindowPaste(event: ClipboardEvent) {
      if (busy) return;
      const items = event.clipboardData?.items;
      if (!items) return;
      const imageFiles: File[] = [];
      for (const item of Array.from(items)) {
        if (item.kind === "file" && item.type.startsWith("image/")) {
          const file = item.getAsFile();
          if (file) imageFiles.push(file);
        }
      }
      if (imageFiles.length > 0) {
        event.preventDefault();
        void addScreenshotFiles(imageFiles);
      }
    }
    window.addEventListener("paste", handleWindowPaste);
    return () => window.removeEventListener("paste", handleWindowPaste);
  }, [busy, addScreenshotFiles]);

  function handleScreenshotInput(event: ChangeEvent<HTMLInputElement>) {
    const files = Array.from(event.target.files ?? []);
    event.target.value = "";
    void addScreenshotFiles(files);
  }

  function handleDrop(event: DragEvent<HTMLDivElement>) {
    event.preventDefault();
    setDragOver(false);
    if (busy) return;
    const files = Array.from(event.dataTransfer.files ?? []);
    void addScreenshotFiles(files);
  }

  function updateLink(index: number, value: string) {
    setLinks((prev) => prev.map((item, idx) => (idx === index ? value : item)));
    setErrors((prev) => ({ ...prev, links: undefined }));
  }

  function addLinkRow() {
    if (links.length >= MAX_LINKS || busy) return;
    setLinks((prev) => [...prev, ""]);
  }

  function removeLinkRow(index: number) {
    if (links.length <= 1 || busy) return;
    setLinks((prev) => prev.filter((_, idx) => idx !== index));
    setErrors((prev) => ({ ...prev, links: undefined }));
  }

  function validateAll(): { valid: boolean; nextErrors: ValidationErrors; cleanedLinks: string[] } {
    const nextErrors: ValidationErrors = {};

    const trimmedModule = moduleName.trim();
    if (!trimmedModule) {
      nextErrors.module = "请填写或选择模块名称";
    } else if (trimmedModule.length > MAX_MODULE_LEN) {
      nextErrors.module = `模块名称不能超过 ${MAX_MODULE_LEN} 字（当前 ${trimmedModule.length} 字）`;
    }

    if (!doc.trim()) {
      nextErrors.doc = "请填写或上传需求文档";
    } else if (doc.length > MAX_DOC_LEN) {
      nextErrors.doc = `需求文档已超过 ${MAX_DOC_LEN.toLocaleString("zh-CN")} 字，请拆分后再提交`;
    }

    const cleanedLinks: string[] = [];
    if (links.length > MAX_LINKS) {
      nextErrors.links = `UI 图链接最多填写 ${MAX_LINKS} 个`;
    } else {
      for (let i = 0; i < links.length; i += 1) {
        const raw = links[i];
        const trimmed = raw.trim();
        if (!trimmed) {
          if (links.length === 1) {
            nextErrors.links = "请至少填写 1 个 UI 图链接";
          } else {
            nextErrors.links = `第 ${i + 1} 行 UI 图链接为空，请填写或删除该行`;
          }
          break;
        }
        if (/\s/.test(trimmed)) {
          nextErrors.links = `第 ${i + 1} 个 UI 图链接不能包含空格`;
          break;
        }
        if (!/^https?:\/\/\S+$/i.test(trimmed)) {
          nextErrors.links = `第 ${i + 1} 个 UI 图链接必须以 http:// 或 https:// 开头`;
          break;
        }
        cleanedLinks.push(trimmed);
      }
      if (!nextErrors.links && cleanedLinks.length === 0) {
        nextErrors.links = "请至少填写 1 个 UI 图链接";
      }
    }

    if (screenshots.length < 1) {
      nextErrors.screenshots = "请至少上传 1 张 UI 区域截图";
    } else if (screenshots.length > MAX_SCREENSHOTS) {
      nextErrors.screenshots = `UI 区域截图最多上传 ${MAX_SCREENSHOTS} 张`;
    } else if (screenshots.reduce((sum, item) => sum + item.size, 0) > MAX_TOTAL_IMAGE_BYTES) {
      nextErrors.screenshots = `截图合计不能超过 ${MAX_TOTAL_IMAGE_BYTES / 1024 / 1024}MB，请少传几张或裁小一点`;
    }

    if (!nodeId) {
      nextErrors.node = "请选择在线执行节点";
    } else if (nodeRolesOffline) {
      nextErrors.node = "所选节点已离线，请先启动节点或切换其他在线节点";
    }

    if (!trimmedName) {
      nextErrors.userName = "请填「我是谁」（你的名字）";
    }

    if (nodeId && trimmedName && !nodeRolesOffline) {
      if (rolesState.loading) {
        nextErrors.writerRole = "正在读取节点角色，请稍候";
      } else if (rolesState.error) {
        nextErrors.writerRole = "节点角色读取失败，无法选择角色";
      } else {
        if (!writerRole || !okWriterRoles.some((r) => r.id === writerRole)) {
          nextErrors.writerRole =
            writerRoles.length === 0
              ? "当前节点没有配置「测试用例」角色，请先前往「角色与模型」配置"
              : "请选择一个检测通过（可用）的测试用例角色";
        }
        if (!reviewerRole || !okJudgeRoles.some((r) => r.id === reviewerRole)) {
          nextErrors.reviewerRole =
            judgeRoles.length === 0
              ? "当前节点没有配置「判定员」角色，请先前往「角色与模型」配置"
              : "请选择一个检测通过（可用）的判定员（评审员）角色";
        }
      }
    }

    return {
      valid: Object.keys(nextErrors).length === 0,
      nextErrors,
      cleanedLinks,
    };
  }

  async function submitInput(input: CasegenInput, enteredToken?: string) {
    if (submitLock.current || (pendingInput && enteredToken === undefined)) return;
    const token = enteredToken ?? readReviewToken();
    const ready = Boolean(nodeId && trimmedName && !nodeRolesOffline && !submitting);
    const step = writeTokenAction(ready, token);
    if (step === "blocked") {
      setPendingInput(null);
      return;
    }
    setSubmitError(null);
    if (step === "prompt") {
      setPendingInput(input);
      return;
    }
    if (enteredToken !== undefined) rememberReviewToken(enteredToken);

    submitLock.current = true;
    setPendingInput(null);
    setSubmitting(true);
    try {
      const result = await createCasegenRequest(project, input, token, trimmedName);
      window.location.hash = `#/modules/${encodeURIComponent(result.module_id)}`;
      onClose();
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : String(err));
    } finally {
      submitLock.current = false;
      setSubmitting(false);
    }
  }

  function handleSubmitClick() {
    if (busy) return;
    setSubmitError(null);
    const { valid, nextErrors, cleanedLinks } = validateAll();
    setErrors(nextErrors);
    if (!valid) return;

    const payload: CasegenInput = {
      module: moduleName.trim(),
      doc,
      ui_links: cleanedLinks,
      screenshots: screenshots.map((item) => ({ data_b64: item.data_b64 })),
      node_id: nodeId,
      writer_role: writerRole,
      reviewer_role: reviewerRole,
    };
    void submitInput(payload);
  }

  const writerOptions: RoleSelectOption[] = useMemo(
    () =>
      writerRoles.map((role) => ({
        value: role.id,
        label: `${role.name}（${CHECK_STATUS_META[role.check.status].text}）`,
        disabled: role.check.status !== "ok",
        role,
      })),
    [writerRoles],
  );

  const judgeOptions: RoleSelectOption[] = useMemo(
    () =>
      judgeRoles.map((role) => ({
        value: role.id,
        label: `${role.name}（${CHECK_STATUS_META[role.check.status].text}）`,
        disabled: role.check.status !== "ok",
        role,
      })),
    [judgeRoles],
  );

  const validationSummary = Object.values(errors).filter(
    (item): item is string => typeof item === "string" && item.length > 0,
  );
  const docOverLimit = doc.length > MAX_DOC_LEN;
  const moduleOverLimit = moduleName.trim().length > MAX_MODULE_LEN;

  return (
    <Modal
      open
      width={760}
      title="新建任务（生成用例）"
      okText={submitting ? "正在提交…" : "提交生成任务"}
      cancelText="取消"
      mask={{ closable: !busy }}
      keyboard={!busy}
      closable={!busy}
      confirmLoading={submitting}
      okButtonProps={{ disabled: busy }}
      cancelButtonProps={{ disabled: busy }}
      onOk={handleSubmitClick}
      onCancel={onClose}
    >
      <div className="mod-gen-form">
        {nodesError && <LoadError title="无法加载节点列表" error={nodesError} interval={REFRESH_MS} />}
        {submitError && (
          <Alert
            type="error"
            showIcon
            title="提交生成任务失败"
            description={<div className="mod-wrap-text">{submitError}</div>}
          />
        )}

        {/* 1. 模块 */}
        <div className="mod-field">
          <div className="mod-field-head">
            <RequiredLabel text="模块" />
            <span className={`mod-field-meta mod-mono${moduleOverLimit ? " is-over" : ""}`}>
              {moduleName.trim().length} / {MAX_MODULE_LEN} 字
            </span>
          </div>
          <AutoComplete
            value={moduleName}
            disabled={busy}
            options={moduleOptions}
            placeholder="选择已有模块，或直接输入新模块名称（不超过 80 字）"
            onChange={(val) => {
              setModuleName(val);
              setErrors((prev) => ({ ...prev, module: undefined }));
            }}
          />
          {errors.module && <div className="mod-field-error">{errors.module}</div>}
        </div>

        {/* 2. 需求文档 */}
        <div className="mod-field">
          <div className="mod-field-head">
            <RequiredLabel text="需求文档" />
            <div className="mod-field-meta">
              <span className={`mod-mono${docOverLimit ? " is-over" : ""}`}>
                {doc.length.toLocaleString("zh-CN")} / {MAX_DOC_LEN.toLocaleString("zh-CN")} 字
              </span>
              <Button
                size="small"
                disabled={busy}
                onClick={() => docFileInputRef.current?.click()}
              >
                上传 .md / .txt
              </Button>
              <input
                ref={docFileInputRef}
                type="file"
                accept=".md,.txt,text/markdown,text/plain"
                className="mod-hidden-input"
                onChange={(e) => void handleDocFileChange(e)}
              />
            </div>
          </div>
          <Input.TextArea
            rows={7}
            disabled={busy}
            value={doc}
            placeholder="直接粘贴需求文档内容，或点击右上角「上传 .md / .txt」载入文本"
            onChange={(event) => {
              setDoc(event.target.value);
              setErrors((prev) => ({ ...prev, doc: undefined }));
            }}
          />
          <div className="mod-field-hint">
            账号密码只能写成 <code className="mod-mono">{"{{account.xxx}}"}</code> 占位符，文档里出现明文密码会被拒绝。
            {docUploadNote ? ` ${docUploadNote}` : ""}
          </div>
          {docOverLimit && (
            <div className="mod-field-error">
              当前文档已超过 200,000 字上限，请按功能拆分后再提交。
            </div>
          )}
          {!docOverLimit && errors.doc && <div className="mod-field-error">{errors.doc}</div>}
        </div>

        {/* 3. UI 图链接 */}
        <div className="mod-field">
          <div className="mod-field-head">
            <RequiredLabel text="UI 图链接" />
            <div className="mod-field-meta">
              <span className="mod-mono">
                {links.length} / {MAX_LINKS} 个
              </span>
              <Button
                size="small"
                disabled={busy || links.length >= MAX_LINKS}
                onClick={addLinkRow}
              >
                + 添加链接
              </Button>
            </div>
          </div>
          <div className="mod-links-list">
            {links.map((link, index) => (
              <div key={index} className="mod-link-row">
                <Input
                  className="mod-link-row-input"
                  disabled={busy}
                  value={link}
                  placeholder={`https://...（第 ${index + 1} 个 UI 图链接，不能含空格）`}
                  onChange={(event) => updateLink(index, event.target.value)}
                />
                <Button
                  size="small"
                  danger
                  disabled={busy || links.length <= 1}
                  onClick={() => removeLinkRow(index)}
                >
                  删除
                </Button>
              </div>
            ))}
          </div>
          <div className="mod-field-hint">
            链接只做记录和跳转，AI 不会打开它，只看下面你上传的截图。
          </div>
          {errors.links && <div className="mod-field-error">{errors.links}</div>}
        </div>

        {/* 4. UI 区域截图 */}
        <div className="mod-field">
          <div className="mod-field-head">
            <RequiredLabel text="UI 区域截图" />
            <span className="mod-field-meta mod-mono">
              已上传 {screenshots.length} / {MAX_SCREENSHOTS} 张（每张 ≤ 3MB，PNG / JPEG）
            </span>
          </div>

          <div
            role="button"
            tabIndex={busy ? -1 : 0}
            aria-label="上传 UI 区域截图，支持点击选择、拖拽或粘贴剪贴板图片"
            className={`mod-dropzone${dragOver ? " is-dragover" : ""}${busy ? " is-disabled" : ""}`}
            onClick={() => {
              if (!busy) imgFileInputRef.current?.click();
            }}
            onKeyDown={(event) => {
              if (!busy && (event.key === "Enter" || event.key === " ")) {
                event.preventDefault();
                imgFileInputRef.current?.click();
              }
            }}
            onDragOver={(event) => {
              event.preventDefault();
              if (!busy) setDragOver(true);
            }}
            onDragLeave={() => setDragOver(false)}
            onDrop={handleDrop}
          >
            <UploadIcon />
            <div className="mod-dropzone-title">
              点击选择图片、拖拽到此处，或直接粘贴剪贴板截图
            </div>
            <div className="mod-dropzone-sub">
              至少 1 张，最多 100 张；仅接受 PNG / JPEG 格式，单张不超过 3MB，合计不超过 100MB
            </div>
          </div>

          <input
            ref={imgFileInputRef}
            type="file"
            accept="image/png,image/jpeg"
            multiple
            className="mod-hidden-input"
            onChange={handleScreenshotInput}
          />

          {screenshotNotice && (
            <Alert
              type="warning"
              showIcon
              closable
              onClose={() => setScreenshotNotice(null)}
              title="部分截图未添加"
              description={screenshotNotice}
            />
          )}

          {screenshots.length > 0 && (
            <div className="mod-thumb-grid">
              {screenshots.map((shot) => (
                <div key={shot.id} className="mod-thumb-card">
                  <img src={shot.previewUrl} alt={shot.name} className="mod-thumb-img" />
                  <div className="mod-thumb-foot">
                    <span className="mod-thumb-name" title={`${shot.name} (${formatBytes(shot.size)})`}>
                      {shot.name}
                    </span>
                    <button
                      type="button"
                      className="mod-thumb-del"
                      disabled={busy}
                      onClick={() =>
                        setScreenshots((prev) => prev.filter((item) => item.id !== shot.id))
                      }
                    >
                      删除
                    </button>
                  </div>
                </div>
              ))}
            </div>
          )}

          {errors.screenshots && <div className="mod-field-error">{errors.screenshots}</div>}
        </div>

        {/* 5. 执行节点与我是谁 */}
        <div className="mod-gen-two-col">
          <div className="mod-field">
            <div className="mod-field-head">
              <RequiredLabel text="执行节点" />
              <span className="mod-field-meta">
                {nodes ? `${onlineNodes.length} 个在线` : "加载中…"}
              </span>
            </div>
            <Select<string>
              value={nodeId || undefined}
              disabled={busy || !nodes || onlineNodes.length === 0}
              placeholder={
                nodes && onlineNodes.length === 0
                  ? "当前没有在线节点"
                  : "选择自己的在线节点"
              }
              options={onlineNodes.map((candidate) => ({
                value: candidate.node_id,
                label: `${candidate.node_id} · ${candidate.caps.os} · ${candidate.devices.length} 台设备`,
              }))}
              onChange={(value) => {
                setNodeId(value);
                setErrors((prev) => ({ ...prev, node: undefined }));
              }}
            />
            {nodes && onlineNodes.length === 0 && (
              <div className="mod-field-error">当前没有在线节点，请先启动自己的执行节点。</div>
            )}
            {nodeRolesOffline && (
              <div className="mod-field-error">所选节点已离线，无法读取角色或发起生成。</div>
            )}
            {!nodeRolesOffline && errors.node && <div className="mod-field-error">{errors.node}</div>}
          </div>

          <div className="mod-field">
            <div className="mod-field-head">
              <RequiredLabel text="我是谁" />
              <IdentityHint className="mod-field-meta" userName={userName} expiresAt={nameExpiresAt}
                nameProblem={submitError === NEED_LOGIN_MESSAGE ? submitError : rolesLoaded ? rolesState.data?.name_problem : null}
                onForget={forgetName} onCreated={setUserName}
                onLogin={() => { setSubmitError(null); setRolesRefreshSeq((n) => n + 1); }} />
            </div>
            <Input
              aria-label="我是谁"
              autoComplete="off"
              maxLength={40}
              allowClear
              disabled={busy}
              value={userName}
              placeholder="你的名字，和启动节点时设的 NODE_USER_NAME 一致"
              onChange={(event) => {
                setUserName(event.target.value);
                setErrors((prev) => ({ ...prev, userName: undefined }));
                if (submitError === NEED_LOGIN_MESSAGE) setSubmitError(null);
              }}
            />
            {errors.userName && <div className="mod-field-error">{errors.userName}</div>}
          </div>
        </div>

        {/* 节点角色读取状态 */}
        {rolesState.error && (
          <Alert
            type="error"
            showIcon
            title="无法读取该节点的角色配置"
            description={<div className="mod-wrap-text">{rolesState.error}</div>}
          />
        )}

        {nodeRolesOffline && nodeId && trimmedName && (
          <Alert
            type="warning"
            showIcon
            title="所选节点当前离线"
            description="节点离线时无法拉取角色列表或执行用例生成，请先确认节点已连上平台。"
          />
        )}

        {/* 流程模板：选了流程，下面的角色自动选好（仍可手动改） */}
        {flows.length > 0 && (
          <div className="mod-field">
            <div className="mod-field-head">
              <RequiredLabel text="流程" />
              <span className="mod-field-meta">在「参数配置」里的流程模板中配置</span>
            </div>
            <Select
              aria-label="流程"
              value={flowCode || undefined}
              disabled={busy}
              placeholder="选择流程"
              options={[...flows.map((f) => ({ value: f.code, label: f.name })), { value: "", label: "不用流程，手动选角色" }]}
              onChange={(val) => setFlowCode(val)}
            />
            {flowNotes.length > 0 && flowCode && (
              <div className="mod-field-hint">{flowNotes.join("；")}。</div>
            )}
          </div>
        )}

        {/* 6. 测试用例角色 & 判定员（评审员）角色 */}
        <div className="mod-gen-two-col">
          <div className="mod-field">
            <div className="mod-field-head">
              <RequiredLabel text="测试用例角色" />
              {nodeId && trimmedName && !nodeRolesOffline && (
                <button
                  type="button"
                  className="mod-thumb-del"
                  style={{ color: "var(--accent)" }}
                  disabled={busy || rolesState.loading}
                  onClick={() => setRolesRefreshSeq((n) => n + 1)}
                >
                  {rolesState.loading ? "读取中…" : "重新读取"}
                </button>
              )}
            </div>
            <Select<string, RoleSelectOption>
              value={writerRole || undefined}
              disabled={busy || !rolesLoaded || nodeRolesOffline || writerRoles.length === 0}
              placeholder={
                !nodeId || !trimmedName
                  ? "先选择节点并填「我是谁」"
                  : rolesState.loading
                    ? "正在读取角色…"
                    : "选择可用的测试用例角色"
              }
              options={writerOptions}
              optionRender={(option) => (
                <div className="mod-role-option">
                  <div className="mod-role-option-main">
                    <span className="mod-role-option-name">{option.data.role.name}</span>
                    <span className="mod-role-option-src">
                      {sourceLabel(option.data.role.source)} · {option.data.role.default.model || "未设模型"}
                    </span>
                  </div>
                  <RoleStatusBadge status={option.data.role.check.status} />
                </div>
              )}
              onChange={(val) => {
                setWriterRole(val);
                setErrors((prev) => ({ ...prev, writerRole: undefined }));
              }}
            />
            {rolesLoaded && !nodeRolesOffline && writerRoles.length === 0 && (
              <div className="mod-field-error">
                该节点还没有配置「测试用例」岗位角色。
                <a className="mod-link" href="#/roles" onClick={onClose}>
                  前往「角色与模型」配置
                </a>
              </div>
            )}
            {rolesLoaded && !nodeRolesOffline && writerRoles.length > 0 && okWriterRoles.length === 0 && (
              <div className="mod-field-error">
                当前「测试用例」角色均未通过检测（已禁用）。请先前往{" "}
                <a className="mod-link" href="#/roles" onClick={onClose}>
                  角色与模型
                </a>{" "}
                完成检测。
              </div>
            )}
            {rolesLoaded && !nodeRolesOffline && okWriterRoles.length > 0 && writerRoles.length > okWriterRoles.length && (
              <div className="mod-field-hint">
                未检测或检测未通过的角色已禁用，可前往{" "}
                <a className="mod-link" href="#/roles" onClick={onClose}>
                  角色与模型
                </a>{" "}
                检测。
              </div>
            )}
            {errors.writerRole && <div className="mod-field-error">{errors.writerRole}</div>}
          </div>

          <div className="mod-field">
            <div className="mod-field-head">
              <RequiredLabel text="评审员（判定员）角色" />
              <span className="mod-field-meta">负责复核生成的用例</span>
            </div>
            <Select<string, RoleSelectOption>
              value={reviewerRole || undefined}
              disabled={busy || !rolesLoaded || nodeRolesOffline || judgeRoles.length === 0}
              placeholder={
                !nodeId || !trimmedName
                  ? "先选择节点并填「我是谁」"
                  : rolesState.loading
                    ? "正在读取角色…"
                    : "选择可用的判定员角色"
              }
              options={judgeOptions}
              optionRender={(option) => (
                <div className="mod-role-option">
                  <div className="mod-role-option-main">
                    <span className="mod-role-option-name">{option.data.role.name}</span>
                    <span className="mod-role-option-src">
                      {sourceLabel(option.data.role.source)} · {option.data.role.default.model || "未设模型"}
                    </span>
                  </div>
                  <RoleStatusBadge status={option.data.role.check.status} />
                </div>
              )}
              onChange={(val) => {
                setReviewerRole(val);
                setErrors((prev) => ({ ...prev, reviewerRole: undefined }));
              }}
            />
            {rolesLoaded && !nodeRolesOffline && judgeRoles.length === 0 && (
              <div className="mod-field-error">
                该节点还没有配置「判定员」岗位角色。
                <a className="mod-link" href="#/roles" onClick={onClose}>
                  前往「角色与模型」配置
                </a>
              </div>
            )}
            {rolesLoaded && !nodeRolesOffline && judgeRoles.length > 0 && okJudgeRoles.length === 0 && (
              <div className="mod-field-error">
                当前「判定员」角色均未通过检测（已禁用）。请先前往{" "}
                <a className="mod-link" href="#/roles" onClick={onClose}>
                  角色与模型
                </a>{" "}
                完成检测。
              </div>
            )}
            {rolesLoaded && !nodeRolesOffline && okJudgeRoles.length > 0 && judgeRoles.length > okJudgeRoles.length && (
              <div className="mod-field-hint">
                未检测或检测未通过的角色已禁用，可前往{" "}
                <a className="mod-link" href="#/roles" onClick={onClose}>
                  角色与模型
                </a>{" "}
                检测。
              </div>
            )}
            {errors.reviewerRole && <div className="mod-field-error">{errors.reviewerRole}</div>}
          </div>
        </div>

        {/* 提交前醒目提示 */}
        <Alert
          type="warning"
          showIcon
          title="数据外发提示"
          description={
            <div>
              <div>文档和截图会发给你所选角色的模型来源（Claude / Codex）处理。</div>
              {(selectedWriterRoleObj || selectedJudgeRoleObj) && (
                <div style={{ marginTop: 4, fontSize: 12 }}>
                  当前已选：
                  {selectedWriterRoleObj
                    ? `测试用例「${selectedWriterRoleObj.name}」（${sourceLabel(selectedWriterRoleObj.source)}）`
                    : "未选测试用例角色"}
                  {" · "}
                  {selectedJudgeRoleObj
                    ? `评审员「${selectedJudgeRoleObj.name}」（${sourceLabel(selectedJudgeRoleObj.source)}）`
                    : "未选评审员角色"}
                </div>
              )}
            </div>
          }
        />

        {validationSummary.length > 0 && (
          <Alert
            type="error"
            showIcon
            title="请先补全或修正以下必填项"
            description={
              <ul style={{ margin: 0, paddingLeft: 18 }}>
                {validationSummary.map((msg) => (
                  <li key={msg}>{msg}</li>
                ))}
              </ul>
            }
          />
        )}
      </div>

      {pendingInput && (
        <ReviewTokenDialog
          disabled={submitting}
          onConfirm={(token) => void submitInput(pendingInput, token)}
          onCancel={() => setPendingInput(null)}
        />
      )}
    </Modal>
  );
}
