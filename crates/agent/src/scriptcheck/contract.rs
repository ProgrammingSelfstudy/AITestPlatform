use std::collections::{BTreeMap, HashSet};
use std::io::Read;

use serde::{Deserialize, Serialize};

/// Ui 的公开签名与生成脚本可导入的模块。JSON 可来自夹具或契约命令的 stdout。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UiContract {
    pub methods: BTreeMap<String, UiMethod>,
    /// 精确模块名或以 .* 结尾的模块前缀，{case} 用本次生成的用例文件名替换。
    pub allowed_imports: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UiMethod {
    #[serde(alias = "params")]
    pub parameters: Vec<UiParameter>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub doc: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UiParameter {
    pub name: String,
    pub kind: ParameterKind,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterKind {
    #[serde(alias = "POSITIONAL_ONLY")]
    PositionalOnly,
    #[serde(alias = "POSITIONAL_OR_KEYWORD")]
    PositionalOrKeyword,
    #[serde(alias = "KEYWORD_ONLY")]
    KeywordOnly,
}

impl UiContract {
    pub fn from_json(reader: impl Read) -> serde_json::Result<Self> {
        serde_json::from_reader(reader)
    }

    /// 读取运行时命令输出的 ui_api。参数种类和默认值由调用方注入的签名补齐。
    /// 新增方法或参数变化时，要求先补充签名，避免放宽静态校验。
    pub fn from_runtime_json(reader: impl Read, signatures: &Self) -> serde_json::Result<Self> {
        #[derive(Deserialize)]
        struct RuntimeContract {
            ui_api: BTreeMap<String, Vec<String>>,
            allowed_imports: Vec<String>,
        }
        let runtime: RuntimeContract = serde_json::from_reader(reader)?;
        let mut methods = BTreeMap::new();
        for (name, parameters) in runtime.ui_api {
            let signature = signatures
                .methods
                .get(&name)
                .filter(|signature| {
                    signature
                        .parameters
                        .iter()
                        .map(|param| &param.name)
                        .eq(&parameters)
                })
                .ok_or_else(|| serde::de::Error::custom("运行时方法或参数与已知 Ui 签名不符"))?;
            methods.insert(name, signature.clone());
        }
        Ok(Self {
            methods,
            allowed_imports: runtime.allowed_imports,
        })
    }

    pub(super) fn allows_import(&self, module: &str, case_key: &str) -> bool {
        self.allowed_imports.iter().any(|allowed| {
            let allowed = allowed.replace("{case}", case_key);
            if let Some(prefix) = allowed.strip_suffix(".*") {
                module
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('.') && suffix.len() > 1)
            } else {
                allowed == module
            }
        })
    }
}

impl UiMethod {
    pub(super) fn accepts(&self, positional: usize, keywords: &[&str]) -> bool {
        let max_positional = self
            .parameters
            .iter()
            .take_while(|param| param.kind != ParameterKind::KeywordOnly)
            .count();
        if positional > max_positional {
            return false;
        }
        let mut bound = HashSet::new();
        bound.extend(
            self.parameters[..positional]
                .iter()
                .map(|param| param.name.as_str()),
        );
        for kw in keywords {
            if !bound.insert(kw)
                || !self
                    .parameters
                    .iter()
                    .any(|p| p.name == *kw && p.kind != ParameterKind::PositionalOnly)
            {
                return false;
            }
        }
        self.parameters
            .iter()
            .all(|param| !param.required || bound.contains(param.name.as_str()))
    }
}
