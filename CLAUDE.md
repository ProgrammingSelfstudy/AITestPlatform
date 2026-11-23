# AI 测试平台

规划见 `docs/`；选型以技术验证仓库 `~/Documents/code/AIAutomation/docs/AI测试平台规划.md` 十二章为准，改选型前先读完整章。

## 语言分工

- 平台服务、执行节点、设备层：**全部 Rust**。
- 前端：React + TypeScript + Vite + Ant Design（`web/`）。
- **Python 只用于「脚本执行和自动化」这一层**（回归脚本、自愈脚本，P3 阶段）。不要用它写后端，也不要推荐用它做后端相关的东西。

## 必须遵守的规则

1. **除 DeepSeek 外不使用 API Key**，只用本机会员登录：Claude 走 `claude` 命令行，GPT 走 `codex exec`，Gemini 走 `agy` 命令行（只接受个人账号登录，不用 API Key、不用应用默认凭据、不用企业网关）。**唯一例外是 DeepSeek，它只提供 API，允许使用 API Key；其他来源一律不得使用 API Key。**额度用完暂停并告知用户，不自动换计费方式。
2. **凭证安全**：测试账号密码不得出现在日志、轨迹、报告、提交记录里，统一占位符 `{{account.xxx}}`，只在执行节点运行时注入；服务端不把明文密码发给前端。DeepSeek 的 API Key 只存在发起人自己执行节点的本机文件里（权限仅本人可读），服务端、前端、日志、轨迹、报告、提交记录里都不出现；页面只显示「已配置」，不回显。
3. **模型按角色配置**：所有模型调用经角色配置解析，代码里不写死模型名。
4. **卡住检测、步数上限、轨迹记录放在设备 MCP 服务里**，不依赖模型自觉。评审员越出证据工具范围的评审不可信，转「待人工确认」。
5. **判定不能强行通过**：预期与实际不一致必须判失败；无法判断返回「待人工确认」。
6. **高危动作**（支付、删除账号、注销）：工具层直接拒绝并返回 `need_human`。
7. 每个阶段先跑验收检查再进下一阶段；设备命令失败把原始错误给用户看，重试不超过 3 次。
8. 代码注释和输出用中文。
9. **谁发起谁付额度**：任务只能在发起人自己的执行节点上、用发起人自己的模型来源执行，不能派到别人的节点。角色与模型配置、可用性检测都在发起人自己的节点上进行。DeepSeek 按量计费，每个任务必须有费用或步数上限。

## 目录

```
crates/protocol   节点↔服务端消息类型（协议唯一来源）
crates/server     Axum API + WebSocket hub
crates/node       执行节点
crates/device     设备层（adb）
migrations/       SQLx 迁移
web/              前端
docs/             数据模型、节点协议、落地记录
```

## 常用命令

```bash
cargo build --workspace
cargo test --workspace
cargo run -p server
cargo run -p node
cd web && pnpm test    # 前端测试（vitest + jsdom）
```
