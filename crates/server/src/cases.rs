//! 用例：解析技术验证里的用例 YAML、导入数据库、只读查询。
//!
//! 导入是**本机管理操作**（命令行 `server import-cases`），不开放成 HTTP 接口：目前没有鉴权，
//! 一个能读服务器文件系统的接口等于任意文件读取。查询接口只读，内容只有占位符、没有账号明文。
//!
//! 强制的规则：
//! - 用例数据里账号相关的字段必须是 `{{account.xxx}}` 占位符，写了明文就拒绝导入；
//! - 导入全有或全无：任何一个文件有问题，一个都不写；
//! - 重复导入不改版本，只有内容真的变了才把 `version` 加一（run 要记录当时的用例版本）；
//! - 「故意写错的负例」由 `level = 故意失败` 推出，不靠编号后缀。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use protocol::{CaseExpected, CaseSpec};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// 负例的 level 取值。
pub const NEGATIVE_LEVEL: &str = "故意失败";

#[derive(Debug, Deserialize)]
struct RawCase {
    id: String,
    title: String,
    #[serde(default)]
    level: String,
    #[serde(default)]
    platform: Vec<String>,
    #[serde(default)]
    note: String,
    #[serde(default)]
    refs: Vec<String>,
    #[serde(default)]
    preconditions: Vec<String>,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    steps: Vec<String>,
    #[serde(default)]
    expected: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewCase {
    pub code: String,
    pub title: String,
    pub level: String,
    pub platforms: Vec<String>,
    pub preconditions: Vec<String>,
    pub steps: Vec<String>,
    /// `[{idx, text}]`，idx 从 1 开始
    pub expected: Vec<CaseExpected>,
    pub data: Value,
    pub refs: Vec<String>,
    pub note: String,
    pub should_fail: bool,
}

/// 账号相关的键名：值必须是 `{{account.xxx}}` 占位符。
fn is_account_key(k: &str) -> bool {
    let k = k.to_lowercase();
    ["password", "passwd", "pwd", "username", "account", "email", "token", "secret"].iter().any(|w| k.contains(w))
}

fn is_placeholder(v: &Value) -> bool {
    v.as_str().is_some_and(|s| s.starts_with("{{account.") && s.ends_with("}}"))
}

/// 递归检查嵌套的 data：账号键名下的所有叶子值都必须是占位符。
/// `in_account` 表示已经处于账号键名之下。返回第一个违规值的路径（不含值本身）。
fn find_plain_account_value(path: &str, v: &Value, in_account: bool) -> Option<String> {
    let in_account = in_account || path.rsplit(['.', '[']).next().is_some_and(is_account_key);
    match v {
        Value::Object(m) => m.iter().find_map(|(k, v)| find_plain_account_value(&format!("{path}.{k}"), v, in_account)),
        Value::Array(a) => a.iter().enumerate().find_map(|(i, v)| find_plain_account_value(&format!("{path}[{i}]"), v, in_account)),
        // 账号键名下的空值、布尔值不含凭证，放行
        Value::Null | Value::Bool(_) => None,
        _ if in_account && !is_placeholder(v) => Some(path.to_string()),
        _ => None,
    }
}

fn blank(s: &str) -> bool {
    s.trim().is_empty()
}

/// 解析一个用例 YAML。`source` 只用于错误信息（文件名）。
pub fn parse_case(yaml: &str, source: &str) -> Result<NewCase, String> {
    let raw: RawCase = serde_yaml_ng::from_str(yaml).map_err(|e| format!("{source}：YAML 解析失败：{e}"))?;
    let err = |m: &str| Err(format!("{source}：{m}"));
    let code = raw.id.trim().to_string();
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return err(&format!("用例编号 id 不合法：{:?}（只允许字母、数字、- _ .）", raw.id));
    }
    if blank(&raw.title) {
        return err("缺少 title");
    }
    if raw.steps.is_empty() || raw.steps.iter().any(|s| blank(s)) {
        return err("steps 不能为空，也不能有空白步骤");
    }
    if raw.expected.is_empty() || raw.expected.iter().any(|s| blank(s)) {
        return err("expected 不能为空，也不能有空白预期（Judge 要逐条判定）");
    }
    let data = raw.data.unwrap_or_else(|| json!({}));
    let Some(obj) = data.as_object() else { return err("data 必须是键值对") };
    for (k, v) in obj {
        if let Some(path) = find_plain_account_value(k, v, false) {
            // 错误信息里不回显值：它很可能就是明文密码
            return err(&format!("data.{path} 看起来是账号字段，必须写成 {{{{account.xxx}}}} 占位符，不能写明文（值未显示）"));
        }
    }
    Ok(NewCase {
        code,
        title: raw.title.trim().to_string(),
        should_fail: raw.level.trim() == NEGATIVE_LEVEL,
        level: raw.level.trim().to_string(),
        platforms: raw.platform,
        preconditions: raw.preconditions,
        steps: raw.steps,
        expected: raw.expected.into_iter().enumerate().map(|(i, text)| CaseExpected { idx: i as u32 + 1, text }).collect(),
        data,
        refs: raw.refs,
        note: raw.note.trim().to_string(),
    })
}

/// 读取目录下所有 `*.yaml`（不递归，子目录如 `_archive` 不导入）。有任何问题就返回全部错误，一个都不导入。
pub fn parse_dir(dir: &Path) -> Result<Vec<NewCase>, Vec<String>> {
    let rd = std::fs::read_dir(dir).map_err(|e| vec![format!("读不了目录 {}：{e}", dir.display())])?;
    let mut files: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "yaml" || x == "yml")).collect();
    files.sort();
    let (mut cases, mut errors, mut seen): (Vec<NewCase>, Vec<String>, HashMap<String, String>) = (vec![], vec![], HashMap::new());
    for f in files {
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match std::fs::read_to_string(&f).map_err(|e| format!("{name}：读取失败：{e}")).and_then(|t| parse_case(&t, &name)) {
            Ok(c) => {
                if let Some(prev) = seen.insert(c.code.clone(), name.clone()) {
                    errors.push(format!("{name}：用例编号 {} 和 {prev} 重复", c.code));
                } else {
                    cases.push(c);
                }
            }
            Err(e) => errors.push(e),
        }
    }
    if cases.is_empty() && errors.is_empty() {
        errors.push(format!("{} 下没有找到任何 .yaml 用例文件", dir.display()));
    }
    if errors.is_empty() { Ok(cases) } else { Err(errors) }
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct ImportReport {
    pub inserted: Vec<String>,
    /// 内容变了，版本已加一
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    /// 只有状态变了（active / blocked），内容没变，版本不变
    pub status_changed: Vec<String>,
}

/// 导入。`blocked`：用例编号 → 受阻原因（YAML 里没有这个信息，由调用方指定）。整个导入在一个事务里。
pub async fn import(pool: &PgPool, project: &str, cases: &[NewCase], blocked: &BTreeMap<String, String>) -> Result<ImportReport, String> {
    for code in blocked.keys() {
        if !cases.iter().any(|c| &c.code == code) {
            return Err(format!("--block 指定的用例 {code} 不在导入的用例里"));
        }
    }
    let db = |e: sqlx::Error| format!("数据库错误：{e}");
    let mut tx = pool.begin().await.map_err(db)?;
    let project_id: Uuid = sqlx::query_scalar(
        "insert into projects (name) values ($1) on conflict (name) do update set name = excluded.name returning id",
    )
    .bind(project)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;

    let mut report = ImportReport::default();
    for c in cases {
        let expected = serde_json::to_value(&c.expected).expect("可序列化");
        let (status, reason) = match blocked.get(&c.code) {
            Some(r) => ("blocked", Some(r.as_str())),
            None => ("active", None),
        };
        // 内容（不含状态）变了才加版本
        let upsert = sqlx::query(
            "insert into test_cases (project_id, code, title, level, platforms, preconditions, steps, expected, data, refs, note, should_fail, status, blocked_reason)
             values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
             on conflict (project_id, code) do update set
               title = excluded.title, level = excluded.level, platforms = excluded.platforms,
               preconditions = excluded.preconditions, steps = excluded.steps, expected = excluded.expected,
               data = excluded.data, refs = excluded.refs, note = excluded.note, should_fail = excluded.should_fail,
               version = test_cases.version + 1, updated_at = now()
             where (test_cases.title, test_cases.level, test_cases.platforms, test_cases.preconditions, test_cases.steps,
                    test_cases.expected, test_cases.data, test_cases.refs, test_cases.note, test_cases.should_fail)
                   is distinct from
                   (excluded.title, excluded.level, excluded.platforms, excluded.preconditions, excluded.steps,
                    excluded.expected, excluded.data, excluded.refs, excluded.note, excluded.should_fail)
             returning (xmax = 0) as inserted",
        )
        .bind(project_id)
        .bind(&c.code)
        .bind(&c.title)
        .bind(&c.level)
        .bind(&c.platforms)
        .bind(json!(c.preconditions))
        .bind(json!(c.steps))
        .bind(expected)
        .bind(&c.data)
        .bind(json!(c.refs))
        .bind(&c.note)
        .bind(c.should_fail)
        .bind(status)
        .bind(reason)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        match upsert {
            Some(r) if r.get::<bool, _>("inserted") => report.inserted.push(c.code.clone()),
            Some(_) => report.updated.push(c.code.clone()),
            None => {
                // 内容没变：看状态有没有变
                let changed = sqlx::query(
                    "update test_cases set status = $3, blocked_reason = $4, updated_at = now()
                     where project_id = $1 and code = $2 and (status, blocked_reason) is distinct from ($3, $4::text)",
                )
                .bind(project_id)
                .bind(&c.code)
                .bind(status)
                .bind(reason)
                .execute(&mut *tx)
                .await
                .map_err(db)?
                .rows_affected();
                if changed > 0 { report.status_changed.push(c.code.clone()) } else { report.unchanged.push(c.code.clone()) }
            }
        }
    }
    tx.commit().await.map_err(db)?;
    Ok(report)
}

// ---------------------------------------------------------------- 只读查询

#[derive(Debug, Clone, Serialize)]
pub struct CaseSummary {
    pub code: String,
    pub title: String,
    pub level: String,
    pub platforms: Vec<String>,
    pub should_fail: bool,
    pub status: String,
    pub blocked_reason: Option<String>,
    pub version: i32,
    /// 生成的用例要先复核和人工确认（approved）才能执行；导入的老用例恒为 approved
    pub review_status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseDetail {
    #[serde(flatten)]
    pub summary: CaseSummary,
    pub note: String,
    pub refs: Vec<String>,
    /// 下发给节点的形态
    pub spec: CaseSpec,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub name: String,
    pub cases: i64,
}

fn summary(r: &sqlx::postgres::PgRow) -> CaseSummary {
    CaseSummary {
        code: r.get("code"),
        title: r.get("title"),
        level: r.get("level"),
        platforms: r.get("platforms"),
        should_fail: r.get("should_fail"),
        status: r.get("status"),
        blocked_reason: r.get("blocked_reason"),
        version: r.get("version"),
        review_status: r.get("review_status"),
    }
}

pub async fn projects(pool: &PgPool) -> Result<Vec<ProjectSummary>, sqlx::Error> {
    let rows = sqlx::query("select p.name, count(c.id) as n from projects p left join test_cases c on c.project_id = p.id and c.status != 'archived' group by p.name order by p.name")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| ProjectSummary { name: r.get("name"), cases: r.get("n") }).collect())
}

/// 用例编号按「自然顺序」排：TC-02 在 TC-10 前面，TC-06a 在 TC-06b 前面。
fn natural_key(code: &str) -> Vec<(u8, String, u64)> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut digit = false;
    let flush = |cur: &mut String, digit: bool, out: &mut Vec<(u8, String, u64)>| {
        if cur.is_empty() {
            return;
        }
        if digit {
            out.push((0, String::new(), cur.parse().unwrap_or(0)));
        } else {
            out.push((1, cur.to_lowercase(), 0));
        }
        cur.clear();
    };
    for c in code.chars() {
        if c.is_ascii_digit() != digit && !cur.is_empty() {
            flush(&mut cur, digit, &mut out);
        }
        digit = c.is_ascii_digit();
        cur.push(c);
    }
    flush(&mut cur, digit, &mut out);
    out
}

pub async fn list(pool: &PgPool, project: &str) -> Result<Option<Vec<CaseSummary>>, sqlx::Error> {
    if sqlx::query_scalar::<_, i64>("select count(*) from projects where name = $1").bind(project).fetch_one(pool).await? == 0 {
        return Ok(None);
    }
    let rows = sqlx::query(
        "select c.code, c.title, c.level, c.platforms, c.should_fail, c.status, c.blocked_reason, c.version, c.review_status
         from test_cases c join projects p on p.id = c.project_id where p.name = $1 and c.status != 'archived'",
    )
    .bind(project)
    .fetch_all(pool)
    .await?;
    let mut v: Vec<CaseSummary> = rows.iter().map(summary).collect();
    v.sort_by_key(|c| natural_key(&c.code));
    Ok(Some(v))
}

pub async fn detail(pool: &PgPool, project: &str, code: &str) -> Result<Option<CaseDetail>, sqlx::Error> {
    let Some(r) = sqlx::query(
        "select c.code, c.title, c.level, c.platforms, c.should_fail, c.status, c.blocked_reason, c.version, c.review_status,
                c.note, c.refs, c.preconditions, c.steps, c.expected, c.data
         from test_cases c join projects p on p.id = c.project_id where p.name = $1 and c.code = $2",
    )
    .bind(project)
    .bind(code)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let strings = |col: &str| -> Result<Vec<String>, sqlx::Error> {
        serde_json::from_value(r.get::<Value, _>(col)).map_err(|e| sqlx::Error::Decode(Box::new(e)))
    };
    let expected: Vec<CaseExpected> = serde_json::from_value(r.get::<Value, _>("expected")).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let s = summary(&r);
    Ok(Some(CaseDetail {
        spec: CaseSpec { code: s.code.clone(), title: s.title.clone(), preconditions: strings("preconditions")?, steps: strings("steps")?, expected, data: r.get("data") },
        note: r.get("note"),
        refs: strings("refs")?,
        summary: s,
    }))
}

/// 按用例 id 取下发形态和版本（创建 run 时做快照用）。
pub async fn spec_by_id(conn: &mut PgConnection, id: Uuid) -> Result<(CaseSpec, i32), sqlx::Error> {
    let r = sqlx::query("select code, title, preconditions, steps, expected, data, version from test_cases where id = $1").bind(id).fetch_one(conn).await?;
    let dec = |col: &str| serde_json::from_value::<Vec<String>>(r.get::<Value, _>(col)).map_err(|e| sqlx::Error::Decode(Box::new(e)));
    let expected: Vec<CaseExpected> = serde_json::from_value(r.get::<Value, _>("expected")).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok((CaseSpec { code: r.get("code"), title: r.get("title"), preconditions: dec("preconditions")?, steps: dec("steps")?, expected, data: r.get("data") }, r.get("version")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
id: TC-09
title: 关闭音乐二次确认（预期结果故意写错）
level: 故意失败
platform: [android]
note: |
  本用例的 expected 被故意写错。
refs:
  - 03房间音乐播放 §8.4
preconditions:
  - 已登录并可开语聊房
data:
  account: "{{account.username}}"
  password: "{{account.password}}"
  song: tst_normal_01
steps:
  - 播放 tst_normal_01
  - 点击电源图标
expected:
  # 注释会被忽略
  - 点击关闭音乐后音乐立即停止
  - 房间直接回到无音乐状态
"#;

    #[test]
    fn 解析一个用例() {
        let c = parse_case(GOOD, "TC-09.yaml").unwrap();
        assert_eq!((c.code.as_str(), c.platforms.as_slice()), ("TC-09", &["android".to_string()][..]));
        assert_eq!(c.steps.len(), 2);
        assert_eq!(c.expected.iter().map(|e| e.idx).collect::<Vec<_>>(), [1, 2], "预期编号从 1 开始");
        assert_eq!(c.data["song"], "tst_normal_01");
        assert!(c.note.starts_with("本用例的 expected"));
    }

    #[test]
    fn 负例由level推出而不是编号后缀() {
        assert!(parse_case(GOOD, "x").unwrap().should_fail, "TC-09 没有 -F 后缀，但 level 是故意失败");
        let normal = GOOD.replace("level: 故意失败", "level: 简单流程").replace("TC-09", "TC-02-F");
        assert!(!parse_case(&normal, "x").unwrap().should_fail, "编号带 -F 但 level 不是故意失败，不算负例");
    }

    #[test]
    fn 账号字段写明文被拒绝且错误里不回显值() {
        let bad = GOOD.replace(r#"password: "{{account.password}}""#, r#"password: "Hunter2Plain""#);
        let e = parse_case(&bad, "TC-09.yaml").unwrap_err();
        assert!(e.contains("data.password") && e.contains("占位符"), "{e}");
        assert!(!e.contains("Hunter2Plain"), "错误信息不能回显疑似明文密码：{e}");
    }

    #[test]
    fn 嵌套的账号字段写明文也被拒绝且不回显值() {
        for (snippet, path) in [
            ("login:\n    password: Hunter2Plain", "data.login.password"),
            ("users:\n    - name: a\n      pwd: Hunter2Plain", "data.users[0].pwd"),
            ("account:\n    detail:\n      - Hunter2Plain", "data.account.detail[0]"),
        ] {
            let y = GOOD.replace("song: tst_normal_01", snippet);
            let e = parse_case(&y, "TC-09.yaml").unwrap_err();
            assert!(e.contains(path), "{path}：{e}");
            assert!(!e.contains("Hunter2Plain"), "错误信息不能回显明文：{e}");
        }
        let ok = GOOD.replace("song: tst_normal_01", "login:\n    password: \"{{account.password}}\"\n    remember: true");
        assert!(parse_case(&ok, "x").is_ok(), "嵌套占位符放行");
    }

    #[test]
    fn 各种账号相关键名都要检查() {
        for key in ["username", "user_password", "Email", "api_token", "client_secret", "account"] {
            let y = GOOD.replace("song: tst_normal_01", &format!("{key}: plain-value"));
            assert!(parse_case(&y, "x").is_err(), "键 {key} 写明文必须被拒绝");
        }
        let ok = GOOD.replace("song: tst_normal_01", "room_name: AI Auto Test Room");
        assert!(parse_case(&ok, "x").is_ok(), "和账号无关的键不受影响");
    }

    #[test]
    fn 缺少必要字段被拒绝并指出文件() {
        let no_steps = GOOD.replace("steps:\n  - 播放 tst_normal_01\n  - 点击电源图标\n", "steps: []\n");
        assert!(parse_case(&no_steps, "TC-X.yaml").unwrap_err().starts_with("TC-X.yaml："));
        let no_exp = GOOD.replace("expected:\n  # 注释会被忽略\n  - 点击关闭音乐后音乐立即停止\n  - 房间直接回到无音乐状态\n", "expected: []\n");
        assert!(parse_case(&no_exp, "x").unwrap_err().contains("expected"));
        assert!(parse_case(&GOOD.replace("title: 关闭音乐二次确认（预期结果故意写错）", "title: ' '"), "x").is_err());
        assert!(parse_case("id: TC-1\ntitle: t\nsteps: ['  ']\nexpected: [a]", "x").is_err(), "空白步骤不行");
    }

    #[test]
    fn 编号不合法被拒绝() {
        for id in ["", "TC 04", "TC/04", "../x"] {
            assert!(parse_case(&GOOD.replace("id: TC-09", &format!("id: '{id}'")), "x").is_err(), "{id:?}");
        }
    }

    #[test]
    fn yaml语法错误给出文件名() {
        let e = parse_case("id: [未闭合", "bad.yaml").unwrap_err();
        assert!(e.starts_with("bad.yaml：YAML 解析失败"), "{e}");
    }

    #[test]
    fn 自然顺序排序() {
        let mut v = vec!["TC-10", "TC-02", "TC-02-F", "TC-06b", "TC-06a", "TC-1", "TC-06c-F", "TC-06c"];
        v.sort_by_key(|c| natural_key(c));
        assert_eq!(v, ["TC-1", "TC-02", "TC-02-F", "TC-06a", "TC-06b", "TC-06c", "TC-06c-F", "TC-10"]);
    }

    #[test]
    fn 目录解析_全有或全无且报出所有错误() {
        let d = std::env::temp_dir().join(format!("cases_dir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("_archive")).unwrap();
        std::fs::write(d.join("a.yaml"), GOOD).unwrap();
        std::fs::write(d.join("b.yaml"), GOOD).unwrap(); // 编号重复
        std::fs::write(d.join("c.yaml"), "id: TC-3\ntitle: t\nsteps: []\nexpected: [x]").unwrap(); // 缺 steps
        std::fs::write(d.join("_archive/old.yaml"), "垃圾").unwrap(); // 子目录不导入
        std::fs::write(d.join("notes.txt"), "忽略").unwrap();
        let errs = parse_dir(&d).unwrap_err();
        assert_eq!(errs.len(), 2, "两个问题都要报出来，不能只报第一个：{errs:?}");
        assert!(errs.iter().any(|e| e.contains("重复")) && errs.iter().any(|e| e.contains("c.yaml")));
        std::fs::remove_file(d.join("b.yaml")).unwrap();
        std::fs::remove_file(d.join("c.yaml")).unwrap();
        assert_eq!(parse_dir(&d).unwrap().len(), 1, "子目录和非 yaml 文件不导入");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn 空目录明确报错() {
        let d = std::env::temp_dir().join(format!("cases_empty_{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(parse_dir(&d).unwrap_err()[0].contains("没有找到任何"));
        assert!(parse_dir(Path::new("/不存在的目录")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
