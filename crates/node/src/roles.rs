//! 节点本机的角色与模型配置（规则 9：配置和检测都在发起人自己的节点上）。
//!
//! - `roles.json`：角色配置，不含 Key。
//! - `role_keys.json`：各角色的 API Key（目前只有 DeepSeek 用），**权限 0600，仅本人可读**。
//! - Key 永远不上报、不写日志：上报给服务端的摘要里只有 `key_configured` 布尔。
//! - 检测结果只放内存；角色配置或 Key 变了就回到「未检测」。

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use protocol::{CheckStatus, ModelSource, RoleCheck, RoleConfig, RoleReport, RoleSet, RolesReport};

/// 探测函数：来源 + 模型 → 检测结果。默认调真实命令行，测试里换成假的。
pub type ProbeFn = Arc<dyn Fn(ModelSource, Option<String>) -> Pin<Box<dyn Future<Output = agent::probe::Probe> + Send>> + Send + Sync>;

pub fn real_probe() -> ProbeFn {
    Arc::new(|source, model| Box::pin(async move { agent::probe::check(source, model.as_deref()).await }))
}

pub struct RoleStore {
    dir: PathBuf,
    state: Mutex<State>,
    probe: ProbeFn,
}

#[derive(Default)]
struct State {
    roles: Vec<RoleConfig>,
    keys: BTreeMap<String, String>,
    checks: HashMap<String, RoleCheck>,
}

fn unchecked() -> RoleCheck {
    RoleCheck { status: CheckStatus::Unchecked, detail: None, checked_at: None }
}

fn write_private(path: &Path, content: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        file.write_all(content)?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

impl RoleStore {
    pub fn open(dir: &Path, probe: ProbeFn) -> Self {
        let roles = std::fs::read(dir.join("roles.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let keys = std::fs::read(dir.join("role_keys.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { dir: dir.to_path_buf(), state: Mutex::new(State { roles, keys, checks: HashMap::new() }), probe }
    }

    /// 给服务端的摘要。**不含 Key**。
    pub fn report(&self) -> RolesReport {
        let s = self.state.lock().unwrap();
        RolesReport {
            roles: s
                .roles
                .iter()
                .map(|r| RoleReport {
                    config: r.clone(),
                    key_configured: s.keys.contains_key(&r.id),
                    check: s.checks.get(&r.id).cloned().unwrap_or_else(unchecked),
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().unwrap().roles.is_empty()
    }

    /// 整体替换。`api_key` 为 `None` 的角色保持原来的 Key；被删掉的角色连 Key 一起删。
    pub fn set(&self, sets: Vec<RoleSet>) -> io::Result<()> {
        let mut s = self.state.lock().unwrap();
        let mut keys = BTreeMap::new();
        let mut checks = HashMap::new();
        for set in &sets {
            let id = &set.config.id;
            let key = match set.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
                Some(new) => Some(new.to_string()),
                None => s.keys.get(id).cloned(),
            };
            let same = s.roles.iter().any(|old| old == &set.config) && key == s.keys.get(id).cloned();
            if let Some(key) = key {
                keys.insert(id.clone(), key);
            }
            if same {
                if let Some(check) = s.checks.get(id) {
                    checks.insert(id.clone(), check.clone());
                }
            }
        }
        let roles: Vec<RoleConfig> = sets.into_iter().map(|set| set.config).collect();
        std::fs::create_dir_all(&self.dir)?;
        // 先写 Key 再写配置：中途失败时不会出现「有角色没 Key」的新状态
        write_private(&self.dir.join("role_keys.json"), &serde_json::to_vec(&keys).expect("序列化不会失败"))?;
        write_private(&self.dir.join("roles.json"), &serde_json::to_vec_pretty(&roles).expect("序列化不会失败"))?;
        *s = State { roles, keys, checks };
        Ok(())
    }

    /// 开始检测：把状态改成「检测中」，返回要探测的来源和默认模型。角色不存在或正在检测中返回 `None`。
    pub fn begin_check(&self, role_id: &str) -> Option<(ModelSource, Option<String>)> {
        let mut s = self.state.lock().unwrap();
        let role = s.roles.iter().find(|r| r.id == role_id)?;
        if s.checks.get(role_id).is_some_and(|c| c.status == CheckStatus::Checking) {
            return None;
        }
        let target = (role.source, Some(role.default.model.clone()));
        s.checks.insert(role_id.to_string(), RoleCheck { status: CheckStatus::Checking, detail: None, checked_at: None });
        Some(target)
    }

    pub async fn run_probe(&self, source: ModelSource, model: Option<String>) -> agent::probe::Probe {
        (self.probe)(source, model).await
    }

    /// 记下检测结果。检测期间角色被删掉或改了配置就丢弃这次结果。
    pub fn finish_check(&self, role_id: &str, source: ModelSource, model: &Option<String>, result: agent::probe::Probe) {
        let mut s = self.state.lock().unwrap();
        let still_same = s.roles.iter().any(|r| r.id == role_id && r.source == source && Some(&r.default.model) == model.as_ref());
        if !still_same {
            s.checks.remove(role_id);
            return;
        }
        s.checks.insert(
            role_id.to_string(),
            RoleCheck { status: result.status, detail: Some(result.detail), checked_at: Some(chrono::Utc::now().to_rfc3339()) },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{RoleDefault, RoleModelOption, RoleSlot};

    fn cfg(id: &str, model: &str) -> RoleConfig {
        RoleConfig {
            id: id.into(),
            slot: RoleSlot::Judge,
            name: id.into(),
            source: ModelSource::Agy,
            default: RoleDefault { model: model.into(), effort: None },
            models: vec![RoleModelOption { model: model.into(), efforts: vec![] }],
        }
    }

    fn set(id: &str, model: &str, key: Option<&str>) -> RoleSet {
        RoleSet { config: cfg(id, model), api_key: key.map(str::to_owned) }
    }

    fn fake(status: CheckStatus) -> ProbeFn {
        Arc::new(move |_, _| Box::pin(async move { agent::probe::Probe { status, detail: "假探测".into() } }))
    }

    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!("roles_test_{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn 保存后重开能读回_摘要里没有key() {
        let d = dir();
        let store = RoleStore::open(&d, fake(CheckStatus::Ok));
        store.set(vec![set("a", "m1", Some("sk-绝不能上报"))]).unwrap();
        let again = RoleStore::open(&d, fake(CheckStatus::Ok));
        let report = again.report();
        assert_eq!(report.roles.len(), 1);
        assert!(report.roles[0].key_configured);
        let text = serde_json::to_string(&report).unwrap();
        assert!(!text.contains("sk-绝不能上报"), "摘要泄露了 Key：{text}");
        let roles_file = std::fs::read_to_string(d.join("roles.json")).unwrap();
        assert!(!roles_file.contains("sk-绝不能上报"), "roles.json 里不能有 Key");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn key文件权限只有本人可读() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir();
        RoleStore::open(&d, fake(CheckStatus::Ok)).set(vec![set("a", "m", Some("k"))]).unwrap();
        let mode = std::fs::metadata(d.join("role_keys.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn 不带key保存时保持原key_删掉角色连key一起删() {
        let d = dir();
        let store = RoleStore::open(&d, fake(CheckStatus::Ok));
        store.set(vec![set("a", "m", Some("k1")), set("b", "m", Some("k2"))]).unwrap();
        store.set(vec![set("a", "m", None)]).unwrap();
        let report = store.report();
        assert_eq!(report.roles.len(), 1);
        assert!(report.roles[0].key_configured, "不带 Key 保存应保持原来的");
        let keys = std::fs::read_to_string(d.join("role_keys.json")).unwrap();
        assert!(keys.contains("k1") && !keys.contains("k2"), "被删角色的 Key 要一起删：{keys}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn 检测流程_改配置后回到未检测() {
        let d = dir();
        let store = RoleStore::open(&d, fake(CheckStatus::QuotaExhausted));
        store.set(vec![set("a", "m", None)]).unwrap();
        let (source, model) = store.begin_check("a").unwrap();
        assert_eq!(store.report().roles[0].check.status, CheckStatus::Checking);
        assert!(store.begin_check("a").is_none(), "检测中不能重复开始");
        let result = store.run_probe(source, model.clone()).await;
        store.finish_check("a", source, &model, result);
        let check = store.report().roles[0].check.clone();
        assert_eq!((check.status, check.detail.as_deref()), (CheckStatus::QuotaExhausted, Some("假探测")));
        assert!(check.checked_at.is_some());
        // 配置没变：保留结果；改了模型：回到未检测
        store.set(vec![set("a", "m", None)]).unwrap();
        assert_eq!(store.report().roles[0].check.status, CheckStatus::QuotaExhausted);
        store.set(vec![set("a", "另一个模型", None)]).unwrap();
        assert_eq!(store.report().roles[0].check.status, CheckStatus::Unchecked);
        assert!(store.begin_check("不存在").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn 检测期间配置被改_结果丢弃() {
        let d = dir();
        let store = RoleStore::open(&d, fake(CheckStatus::Ok));
        store.set(vec![set("a", "m", None)]).unwrap();
        let (source, model) = store.begin_check("a").unwrap();
        store.set(vec![set("a", "新模型", None)]).unwrap();
        store.finish_check("a", source, &model, agent::probe::Probe { status: CheckStatus::Ok, detail: "旧结果".into() });
        assert_eq!(store.report().roles[0].check.status, CheckStatus::Unchecked, "旧配置的检测结果不能算到新配置头上");
        let _ = std::fs::remove_dir_all(&d);
    }
}
