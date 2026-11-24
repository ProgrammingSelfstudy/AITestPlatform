import { Alert, Button, Input, Modal, Radio, Upload } from "antd";
import { useState } from "react";
import { fetchConfigExport, importConfigs, type ConfigDocument, type ImportResult } from "./api";

const MAX_FILE_BYTES = 2 * 1024 * 1024;

/** 把 JSON 里的结构粗查一遍，给出中文提示（细的规则由服务端整体校验，任何一条不合格整个文件都不写） */
export function parseConfigDocument(text: string): { document?: ConfigDocument; error?: string } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return { error: "不是有效的 JSON，请检查括号和逗号" };
  }
  const types = (parsed as { types?: unknown })?.types;
  if (!Array.isArray(types) || types.length === 0) return { error: "文件里要有 types 数组，至少一个配置类型" };
  return { document: parsed as ConfigDocument };
}

/** 导入配置：选 JSON 文件或粘贴内容，选「跳过」或「覆盖」已有编码；可以先下载当前配置当模板。 */
export default function ConfigImportDialog({ open, userName, onClose, onDone }: { open: boolean; userName: string; onClose: () => void; onDone: () => void }) {
  const [text, setText] = useState("");
  const [mode, setMode] = useState<"skip" | "overwrite">("skip");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<ImportResult | null>(null);

  function close() {
    setText("");
    setError(null);
    setResult(null);
    onClose();
  }

  async function downloadTemplate() {
    setError(null);
    try {
      const doc = await fetchConfigExport();
      const url = URL.createObjectURL(new Blob([JSON.stringify(doc, null, 2)], { type: "application/json" }));
      const link = document.createElement("a");
      link.href = url;
      link.download = "configs.json";
      link.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      setError(e instanceof Error ? e.message : "下载失败");
    }
  }

  async function submit() {
    setError(null);
    const { document: doc, error: problem } = parseConfigDocument(text);
    if (!doc) {
      setError(problem ?? "文件格式不对");
      return;
    }
    setBusy(true);
    try {
      setResult(await importConfigs(doc, mode, userName));
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : "导入失败");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      title="导入配置"
      open={open}
      onCancel={close}
      destroyOnHidden
      footer={result ? <Button type="primary" onClick={close}>完成</Button> : [
        <Button key="cancel" onClick={close}>取消</Button>,
        <Button key="ok" type="primary" loading={busy} disabled={!text.trim()} onClick={() => void submit()}>导入</Button>,
      ]}
      width={640}
    >
      {result ? (
        <Alert
          type="success"
          showIcon
          title="导入完成"
          description={`类型：新建 ${result.created_types}、覆盖 ${result.updated_types}、跳过 ${result.skipped_types}；配置项：新建 ${result.created_items}、覆盖 ${result.updated_items}、跳过 ${result.skipped_items}`}
        />
      ) : (
        <div className="config-import">
          <p className="page-desc">选一个 JSON 文件，或把内容粘贴到下面。先点「下载当前配置」可以拿到格式模板。文件里任何一条不合格，整个文件都不会导入。</p>
          <div className="config-toolbar">
            <Upload
              accept=".json,application/json"
              showUploadList={false}
              beforeUpload={(file) => {
                if (file.size > MAX_FILE_BYTES) {
                  setError("文件超过 2MB");
                } else {
                  void file.text().then((content) => { setText(content); setError(null); });
                }
                return false;
              }}
            >
              <Button>选择 JSON 文件</Button>
            </Upload>
            <Button onClick={() => void downloadTemplate()}>下载当前配置</Button>
          </div>
          <Input.TextArea rows={10} value={text} onChange={(e) => setText(e.target.value)} placeholder='{"types": [{"name": "类型名", "code": "type_code", "items": [{"name": "项", "code": "item_code", "value": "1"}]}]}' aria-label="配置 JSON" className="mono" />
          <Radio.Group value={mode} onChange={(e) => setMode(e.target.value as "skip" | "overwrite")}>
            <Radio value="skip">已有的编码跳过</Radio>
            <Radio value="overwrite">已有的编码用文件覆盖</Radio>
          </Radio.Group>
          {error && <Alert type="error" showIcon title={error} />}
        </div>
      )}
    </Modal>
  );
}
