//! 组装 `codex exec` 命令行：把 Codex 锁成「只能用设备 MCP 工具」。
//!
//! 锁死的手段（技术验证里验证过的做法，不是凭感觉写的）：
//! - `--ignore-user-config` + `--strict-config`：不读用户本机的 Codex 配置，配置写错直接报错；
//! - 模型目录里把内置工具（shell、apply_patch、搜索……）关掉；
//! - 只声明一个 `device` MCP 服务，并用 `enabled_tools` 限定到白名单；
//! - 沙箱 `read-only`、审批策略 `never`、关闭联网搜索、关掉一长串可选功能（`--disable`）；
//! - 子进程环境变量白名单：只放行 HOME / PATH / 代理等，`OPENAI_*` 之类一概不传。
//!
//! 光靠配置不够，运行前还要用 [`super::audit`] 核对 Codex 实际声明的工具清单，运行中 [`super::events::handle`] 再兜底。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Command;

/// 传给 codex 子进程的环境变量白名单。其它一概不传（尤其是 `OPENAI_*`，会绕过会员登录）。
pub const ENV_KEEP: [&str; 12] =
    ["HOME", "PATH", "TMPDIR", "LANG", "LC_ALL", "CODEX_HOME", "SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY"];

/// 按白名单过滤环境变量。
pub fn filter_env(vars: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    vars.into_iter().filter(|(k, _)| ENV_KEEP.contains(&k.as_str())).collect()
}

/// 要关掉的 Codex 可选功能。
pub const FEATURES: [&str; 22] = [
    "shell_tool", "unified_exec", "view_image", "apps", "plugins", "image_generation", "multi_agent", "multi_agent_v2",
    "goals", "sleep_tool", "tool_suggest", "code_mode", "code_mode_host", "browser_use", "computer_use", "hooks",
    "skill_search", "incremental_tools", "skill_mcp_dependency_install", "unbounded_connection_retries",
    "collaboration_modes", "default_mode_request_user_input",
];

/// `codex` 命令行。`pre` 放在可执行文件后面、子命令前面：包装式调用用；测试里用 `/bin/sh` + 脚本路径模拟。
#[derive(Debug, Clone)]
pub struct Bin {
    pub program: String,
    pub pre: Vec<String>,
}

impl Default for Bin {
    fn default() -> Self {
        Self { program: "codex".into(), pre: vec![] }
    }
}

impl Bin {
    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.pre);
        c
    }
}

/// 设备 MCP 服务怎么启动。
#[derive(Debug, Clone)]
pub struct DeviceMcp {
    pub command: PathBuf,
    /// 传给设备服务的环境变量（HAFA_SERIAL、HAFA_RUN_DIR 等）
    pub env: Vec<(String, String)>,
}

pub struct Spec<'a> {
    pub model: &'a str,
    pub work: &'a Path,
    pub catalog: &'a Path,
    pub instructions: &'a Path,
    pub effort: Option<&'a str>,
    pub mcp: &'a DeviceMcp,
    pub allowed: &'a [String],
    /// 设备服务进程里的 PATH
    pub path_env: &'a str,
}

/// `-c key=value` 的配置项，value 按 JSON 编码。值为 null 的不传。
pub fn settings(s: &Spec) -> Vec<(String, Value)> {
    let mut v: Vec<(String, Value)> = vec![
        ("model_catalog_json".into(), json!(s.catalog.to_string_lossy())),
        ("model_instructions_file".into(), json!(s.instructions.to_string_lossy())),
        ("model_provider".into(), json!("openai")),
        ("forced_login_method".into(), json!("chatgpt")),
        ("approval_policy".into(), json!("never")),
        ("model_reasoning_effort".into(), s.effort.map_or(Value::Null, |e| json!(e))),
        ("web_search".into(), json!("disabled")),
        ("tools.update_plan.enabled".into(), json!(false)),
        ("tools.experimental_request_user_input.enabled".into(), json!(false)),
        ("project_doc_max_bytes".into(), json!(0)),
        ("history.persistence".into(), json!("none")),
        ("mcp_servers.device.command".into(), json!(s.mcp.command.to_string_lossy())),
        ("mcp_servers.device.required".into(), json!(true)),
        ("mcp_servers.device.args".into(), json!([])),
        ("mcp_servers.device.enabled_tools".into(), json!(s.allowed)),
        ("mcp_servers.device.startup_timeout_sec".into(), json!(30)),
        // need_human 会阻塞等人工，默认的 60 秒会把它掐断
        ("mcp_servers.device.tool_timeout_sec".into(), json!(1900)),
        ("mcp_servers.device.default_tools_approval_mode".into(), json!("approve")),
        ("mcp_servers.device.env.PATH".into(), json!(s.path_env)),
    ];
    for (k, val) in &s.mcp.env {
        v.push((format!("mcp_servers.device.env.{k}"), json!(val)));
    }
    v.into_iter().filter(|(_, val)| !val.is_null()).collect()
}

/// 脱敏清单文件路径的环境变量名。设备服务读完立即删除文件，所以这个文件只够一次启动用。
pub const SECRETS_ENV: &str = "HAFA_SECRETS_FILE";

/// 审计用的参数：和真实运行完全一致，**只是不给设备服务脱敏清单**。
///
/// 审计要启动一次设备服务来读工具清单，真实会话还要再启动一次。脱敏清单读完即删，
/// 审计在前就会把它消耗掉，真实会话的设备服务读不到清单，握手前就退出。
/// 审计只列工具、不执行任何动作，不需要脱敏清单；真实会话仍然完整带上，不放宽「读完即删」。
pub fn build_audit_args(s: &Spec) -> Vec<String> {
    let mcp = DeviceMcp { command: s.mcp.command.clone(), env: s.mcp.env.iter().filter(|(k, _)| k != SECRETS_ENV).cloned().collect() };
    let spec = Spec { model: s.model, work: s.work, catalog: s.catalog, instructions: s.instructions, effort: s.effort, mcp: &mcp, allowed: s.allowed, path_env: s.path_env };
    build_args(&spec, None)
}

/// 组装参数。首次运行用 `codex exec`；卡住后用 `codex exec resume <id>` 接着做
/// （需要会话落盘，所以不能带 `--ephemeral`；resume 子命令不接受 `-s` / `-C`，沙箱用 `-c`，工作目录靠进程 cwd）。
pub fn build_args(s: &Spec, resume_id: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = if resume_id.is_some() {
        ["exec", "resume", "--json", "--ignore-user-config", "--skip-git-repo-check", "--strict-config", "-m", s.model, "-c", "sandbox_mode=\"read-only\""]
            .map(String::from)
            .into()
    } else {
        let mut v: Vec<String> =
            ["exec", "--json", "--ignore-user-config", "--skip-git-repo-check", "--strict-config", "-s", "read-only", "-C"].map(String::from).into();
        v.push(s.work.to_string_lossy().into_owned());
        v.extend(["-m".into(), s.model.to_string()]);
        v
    };
    for (k, val) in settings(s) {
        a.extend(["-c".into(), format!("{k}={}", serde_json::to_string(&val).expect("JSON 可序列化"))]);
    }
    for f in FEATURES {
        a.extend(["--disable".into(), f.into()]);
    }
    if let Some(id) = resume_id {
        a.push(id.into());
    }
    a
}

fn codex_home(env: &[(String, String)]) -> PathBuf {
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    get("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(get("HOME").unwrap_or_default()).join(".codex"))
}

pub fn codex_home_of(env: &[(String, String)]) -> PathBuf {
    codex_home(env)
}

/// 生成只含一个模型、且关掉所有内置工具的模型目录文件，返回 (路径, 模型 slug)。
/// 先读本机的 `models_cache.json`，没有（或没有这个模型）再问 `codex debug models`。
/// 配置了模型却找不到时报错，**不悄悄换模型**。
pub async fn catalog(bin: &Bin, work: &Path, model: Option<&str>, env: &[(String, String)]) -> Result<(PathBuf, String), String> {
    let cache = codex_home(env).join("models_cache.json");
    let from_cache = |txt: &str| serde_json::from_str::<Value>(txt).ok().and_then(|v| v.get("models").and_then(Value::as_array).cloned());
    let pick = |entries: &[Value]| {
        entries.iter().find(|e| model.is_none_or(|m| e.get("slug").and_then(Value::as_str) == Some(m)) ).cloned()
    };
    let mut entries = std::fs::read_to_string(&cache).ok().and_then(|t| from_cache(&t)).unwrap_or_default();
    if pick(&entries).is_none() {
        let out = tokio::time::timeout(
            Duration::from_secs(30),
            bin.command().args(["debug", "models"]).env_clear().envs(env.iter().cloned()).current_dir(work).output(),
        )
        .await
        .map_err(|_| "`codex debug models` 超过 30 秒没有返回".to_string())?
        .map_err(|e| format!("无法运行 codex：{e}"))?;
        if !out.status.success() {
            return Err(format!("`codex debug models` 失败：{}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        entries = serde_json::from_slice::<Value>(&out.stdout).ok().and_then(|v| v.get("models").and_then(Value::as_array).cloned()).unwrap_or_default();
    }
    let Some(mut sel) = pick(&entries) else {
        return Err(format!("本机 Codex 模型目录中找不到配置的模型 {}，未更换模型。", model.unwrap_or("（未指定）")));
    };
    let obj = sel.as_object_mut().ok_or("模型目录条目格式不对")?;
    for (k, v) in [
        ("shell_type", json!("disabled")),
        ("apply_patch_tool_type", Value::Null),
        ("tool_mode", json!("direct")),
        ("experimental_supported_tools", json!([])),
        ("node_repl_disabled", json!(true)),
        ("supports_search_tool", json!(false)),
        ("use_responses_lite", json!(false)),
        ("multi_agent_version", Value::Null),
        ("multi_agent_reasoning_effort", Value::Null),
        ("include_apps_usage_instructions", json!(false)),
        ("include_plugin_usage_instructions", json!(false)),
        ("include_skills_usage_instructions", json!(false)),
    ] {
        obj.insert(k.into(), v);
    }
    let slug = obj.get("slug").and_then(Value::as_str).ok_or("模型目录条目缺少 slug")?.to_string();
    let path = work.join("catalog.json");
    std::fs::write(&path, json!({ "models": [sel] }).to_string()).map_err(|e| format!("写模型目录失败：{e}"))?;
    Ok((path, slug))
}

/// 检查本机已用 ChatGPT 会员登录。
pub async fn check_login(bin: &Bin, env: &[(String, String)]) -> Result<(), String> {
    let out = tokio::time::timeout(Duration::from_secs(15), bin.command().args(["login", "status"]).env_clear().envs(env.iter().cloned()).output())
        .await
        .map_err(|_| "`codex login status` 超过 15 秒没有返回".to_string())?
        .map_err(|e| format!("无法运行 codex：{e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() || !text.contains("Logged in using ChatGPT") {
        return Err("未检测到 ChatGPT 会员登录，不运行用例。".into());
    }
    Ok(())
}

/// 删掉本次运行留下的会话文件：续跑要求会话落盘，但不该在用户的 Codex 历史里留垃圾。
pub fn cleanup_sessions(home: &Path, thread_ids: &[String]) {
    fn walk(dir: &Path, id: &str) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, id);
            } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains(id)) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    for id in thread_ids {
        // id 要像个会话 id（十六进制和横线），否则不动：避免空串或奇怪的值把整个目录清空
        if id.len() >= 8 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            walk(&home.join("sessions"), id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(mcp: &'a DeviceMcp, allowed: &'a [String], work: &'a Path) -> Spec<'a> {
        Spec { model: "gpt-x", work, catalog: Path::new("/w/catalog.json"), instructions: Path::new("/w/i.txt"), effort: Some("low"), mcp, allowed, path_env: "/usr/bin" }
    }

    fn fixture() -> (DeviceMcp, Vec<String>) {
        (DeviceMcp { command: "/bin/mcp-device".into(), env: vec![("HAFA_SERIAL".into(), "S1".into()), ("HAFA_RUN_DIR".into(), "/runs/x".into())] }, vec!["observe".into(), "tap".into()])
    }

    fn has_pair(a: &[String], k: &str, v: &str) -> bool {
        a.windows(2).any(|w| w[0] == k && w[1] == v)
    }

    #[test]
    fn 环境变量只放行白名单() {
        let vars = [("HOME", "/h"), ("PATH", "/p"), ("OPENAI_API_KEY", "sk-x"), ("OPENAI_BASE_URL", "http://x"), ("ANTHROPIC_API_KEY", "k"), ("HTTPS_PROXY", "http://proxy"), ("SECRET_TOKEN", "t"), ("CODEX_HOME", "/c")]
            .map(|(k, v)| (k.to_string(), v.to_string()));
        let got: Vec<String> = filter_env(vars).into_iter().map(|(k, _)| k).collect();
        assert_eq!(got, ["HOME", "PATH", "HTTPS_PROXY", "CODEX_HOME"], "OPENAI_* 和其它密钥一律不能传给 codex");
    }

    #[test]
    fn 首次运行的锁死参数() {
        let (mcp, allowed) = fixture();
        let a = build_args(&spec(&mcp, &allowed, Path::new("/w")), None);
        assert_eq!(&a[..2], ["exec", "--json"]);
        for flag in ["--ignore-user-config", "--skip-git-repo-check", "--strict-config"] {
            assert!(a.iter().any(|x| x == flag), "缺少 {flag}");
        }
        assert!(has_pair(&a, "-s", "read-only"), "沙箱必须只读");
        assert!(has_pair(&a, "-C", "/w") && has_pair(&a, "-m", "gpt-x"));
        for (k, v) in [("forced_login_method", "\"chatgpt\""), ("approval_policy", "\"never\""), ("web_search", "\"disabled\""), ("mcp_servers.device.required", "true")] {
            assert!(a.iter().any(|x| x == &format!("{k}={v}")), "缺少 -c {k}={v}");
        }
        assert!(a.iter().any(|x| x == "mcp_servers.device.enabled_tools=[\"observe\",\"tap\"]"), "工具必须限定到白名单");
        assert!(a.iter().any(|x| x == "mcp_servers.device.env.HAFA_SERIAL=\"S1\""), "设备序列号经环境变量传给设备服务");
        assert!(!a.iter().any(|x| x == "resume"));
    }

    #[test]
    fn 关掉全部可选功能() {
        let (mcp, allowed) = fixture();
        let a = build_args(&spec(&mcp, &allowed, Path::new("/w")), None);
        for f in FEATURES {
            assert!(has_pair(&a, "--disable", f), "没有关掉 {f}");
        }
        assert!(FEATURES.contains(&"shell_tool"), "shell 必须在关闭名单里");
    }

    #[test]
    fn 续跑参数用resume且不带c和s() {
        let (mcp, allowed) = fixture();
        let a = build_args(&spec(&mcp, &allowed, Path::new("/w")), Some("aaaabbbb"));
        assert_eq!(&a[..3], ["exec", "resume", "--json"]);
        assert_eq!(a.last().map(String::as_str), Some("aaaabbbb"), "会话 id 放最后");
        assert!(!a.iter().any(|x| x == "-C"), "resume 子命令不接受 -C");
        assert!(!a.iter().any(|x| x == "-s"), "resume 子命令不接受 -s，沙箱要用 -c 指定");
        assert!(has_pair(&a, "-c", "sandbox_mode=\"read-only\""), "续跑也必须只读");
        assert!(a.iter().any(|x| x == "--ignore-user-config") && a.iter().any(|x| x == "--strict-config"), "续跑同样锁死");
    }

    #[test]
    fn 没配置推理强度就不传() {
        let (mcp, allowed) = fixture();
        let mut sp = spec(&mcp, &allowed, Path::new("/w"));
        sp.effort = None;
        let a = build_args(&sp, None);
        assert!(!a.iter().any(|x| x.starts_with("model_reasoning_effort")), "null 的配置项不能传");
    }

    #[test]
    fn 命令行里没有任何api_key参数() {
        let (mcp, allowed) = fixture();
        let a = build_args(&spec(&mcp, &allowed, Path::new("/w")), None).join(" ").to_lowercase();
        assert!(!a.contains("api_key") && !a.contains("apikey") && !a.contains("openai_api"), "{a}");
    }

    fn tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!("cmd_test_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn env_with_cache(models: &str) -> (Vec<(String, String)>, PathBuf) {
        let home = tmp();
        std::fs::write(home.join("models_cache.json"), models).unwrap();
        (vec![("CODEX_HOME".into(), home.to_string_lossy().into()), ("PATH".into(), "/usr/bin:/bin".into())], home)
    }

    #[tokio::test]
    async fn 模型目录关掉所有内置工具() {
        let (env, _) = env_with_cache(r#"{"models":[{"slug":"gpt-x","shell_type":"local","apply_patch_tool_type":"freeform","supports_search_tool":true,"keep":"留着"}]}"#);
        let work = tmp();
        let (path, slug) = catalog(&Bin::default(), &work, Some("gpt-x"), &env).await.unwrap();
        assert_eq!(slug, "gpt-x");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let m = &v["models"][0];
        assert_eq!(m["shell_type"], "disabled", "shell 必须关掉");
        assert!(m["apply_patch_tool_type"].is_null());
        assert_eq!(m["supports_search_tool"], false);
        assert_eq!(m["tool_mode"], "direct");
        assert_eq!(m["keep"], "留着", "其它字段保持原样");
        assert_eq!(v["models"].as_array().unwrap().len(), 1, "目录里只留一个模型");
    }

    #[tokio::test]
    async fn 配置的模型找不到时报错而不是悄悄换模型() {
        // 缓存里没有，再问 codex debug models：这里用不存在的 codex，也要得到明确的错误
        let (env, _) = env_with_cache(r#"{"models":[{"slug":"other"}]}"#);
        let bin = Bin { program: "/不存在/codex".into(), pre: vec![] };
        let e = catalog(&bin, &tmp(), Some("gpt-x"), &env).await.unwrap_err();
        assert!(e.contains("无法运行 codex"), "{e}");
    }

    #[tokio::test]
    async fn 没指定模型取目录里第一个() {
        let (env, _) = env_with_cache(r#"{"models":[{"slug":"first"},{"slug":"second"}]}"#);
        assert_eq!(catalog(&Bin::default(), &tmp(), None, &env).await.unwrap().1, "first");
    }

    #[test]
    fn 清理会话只删匹配的文件() {
        let home = tmp();
        let d = home.join("sessions/2026/11");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("rollout-aaaabbbb.jsonl"), "x").unwrap();
        std::fs::write(d.join("rollout-ccccdddd.jsonl"), "x").unwrap();
        cleanup_sessions(&home, &["aaaabbbb".to_string()]);
        assert!(!d.join("rollout-aaaabbbb.jsonl").exists());
        assert!(d.join("rollout-ccccdddd.jsonl").exists(), "不属于本次运行的会话不能动");
    }

    #[test]
    fn 清理会话遇到可疑id时什么都不删() {
        let home = tmp();
        let d = home.join("sessions");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("keep.jsonl"), "x").unwrap();
        // 空串会匹配所有文件名；带路径符号的 id 也不是合法会话 id
        cleanup_sessions(&home, &[String::new(), "..".into(), "../x".into(), "short".into(), "gggggggg".into()]);
        assert!(d.join("keep.jsonl").exists(), "id 不像会话 id 时一个文件都不能删");
    }
}

#[cfg(test)]
mod audit_args_tests {
    use super::*;

    fn spec<'a>(mcp: &'a DeviceMcp, allowed: &'a [String], work: &'a Path) -> Spec<'a> {
        Spec { model: "gpt-x", work, catalog: Path::new("/w/catalog.json"), instructions: Path::new("/w/i.txt"), effort: Some("low"), mcp, allowed, path_env: "/usr/bin" }
    }

    fn has_key(args: &[String], key: &str) -> bool {
        args.iter().any(|a| a.starts_with(&format!("mcp_servers.device.env.{key}=")))
    }

    #[test]
    fn 审计启动设备服务时不带一次性的脱敏清单_真实会话照常带上() {
        // 回归：审计和真实会话各启动一次设备服务，而脱敏清单读完即删，只够第一次用。
        // 审计在前，把文件消耗掉，真实会话的设备服务就读不到清单、握手前退出，
        // Codex 只报「握手时连接被关闭」。真机验证时每次带账号租约的执行都这样失败。
        let mcp = DeviceMcp {
            command: "/bin/mcp-device".into(),
            env: vec![("HAFA_SERIAL".into(), "S1".into()), ("HAFA_RUN_DIR".into(), "/runs/x".into()), (SECRETS_ENV.into(), "/runs/.secrets/x.json".into())],
        };
        let allowed = vec!["observe".to_string()];
        let s = spec(&mcp, &allowed, Path::new("/w"));

        let real = build_args(&s, None);
        assert!(has_key(&real, SECRETS_ENV), "真实会话必须带脱敏清单，否则没有脱敏就在执行");

        let audit = build_audit_args(&s);
        assert!(!has_key(&audit, SECRETS_ENV), "审计不能带脱敏清单，否则会把一次性文件消耗掉：{audit:?}");
        assert!(!audit.iter().any(|a| a.contains("/runs/.secrets/")), "清单路径不能出现在审计参数里");
        assert!(has_key(&audit, "HAFA_SERIAL") && has_key(&audit, "HAFA_RUN_DIR"), "其余环境变量保持一致，审计看到的工具清单才和真实会话相同");
    }

    #[test]
    fn 没有脱敏清单时审计参数和真实参数完全一致() {
        let mcp = DeviceMcp { command: "/bin/mcp-device".into(), env: vec![("HAFA_SERIAL".into(), "S1".into())] };
        let allowed = vec!["observe".to_string()];
        let s = spec(&mcp, &allowed, Path::new("/w"));
        assert_eq!(build_audit_args(&s), build_args(&s, None), "没有清单时不应有任何差别");
    }
}
