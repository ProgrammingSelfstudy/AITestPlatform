//! 模型调用层。第一期只有本机会员两条路径，**没有 API Key 路径**：
//! - Claude：`claude` 命令行（本机 Claude 会员登录）→ [`claude_cli`]
//! - GPT：`codex exec`（本机 ChatGPT 登录）→ 后续接入
//!
//! 共同约束：启动子进程前必须剔除会绕过会员登录的环境变量；额度用完要暂停并如实上报，
//! 不自动换计费方式；卡住要能被检测并杀掉，不依赖模型自觉。

pub mod agy_cli;
pub mod claude_cli;
pub mod codex_exec;
pub mod judge;
pub mod prompts;
pub mod scriptcheck;
pub mod scriptgen;
