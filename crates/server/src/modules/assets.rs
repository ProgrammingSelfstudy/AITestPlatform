//! 模块详情页的数据：UI 图（可增删）、用例 CSV（导出、导入）、各用例最近一次的录屏。
//!
//! - 数据大家都能看；增删图、导入用例要先登录（有密码的名字）并带审核口令（路由层 `authorize_write`）。
//! - CSV 导入先把整个文件校验一遍，任何一行不合格整个文件都不写，并说明是第几行；通过后在一个事务里写入。
//! - 用例里出现明文密码一律拒绝（规则 2），错误里不回显值。

use axum::http::header;
use axum::response::IntoResponse;

use super::*;

// ---------------------------------------------------------------- 公共

fn module_uuid(id: &str) -> Result<Uuid, ApiError> {
    id.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这个模块".to_string()))
}

async fn require_module(pool: &PgPool, id: Uuid) -> Result<String, ApiError> {
    sqlx::query_scalar("select name from modules where id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| internal("查询模块失败", e))?
        .ok_or((StatusCode::NOT_FOUND, "没有这个模块".to_string()))
}

/// 写操作的发起人：必须是已登录的名字。
async fn writer(state: &ApiState, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    match users::browser_user(state.app.pool(), headers).await.map_err(|e| internal("校验用户失败", e))? {
        BrowserUser::Identified(user) => Ok(user),
        BrowserUser::LoginRequired => Err((StatusCode::UNAUTHORIZED, users::NEED_LOGIN_MESSAGE.into())),
        _ => Err((StatusCode::FORBIDDEN, users::NEED_NAME_MESSAGE.into())),
    }
}

fn bad(message: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}

// ---------------------------------------------------------------- UI 图

#[derive(Serialize)]
pub(crate) struct ShotMeta {
    id: Uuid,
    name: String,
    content_type: String,
    size: i64,
    created_at: DateTime<Utc>,
    created_by: Option<String>,
}

pub(crate) async fn api_list_screenshots(State(state): State<ApiState>, Path(id): Path<String>) -> Result<Json<Vec<ShotMeta>>, ApiError> {
    let module = module_uuid(&id)?;
    let pool = state.app.pool();
    require_module(pool, module).await?;
    let rows = sqlx::query(
        "select s.id, s.name, s.content_type, length(s.bytes)::bigint as size, s.created_at, u.name as created_by
         from module_screenshots s left join users u on u.id = s.created_by
         where s.module_id = $1 order by s.created_at, s.id",
    )
    .bind(module)
    .fetch_all(pool)
    .await
    .map_err(|e| internal("查询 UI 图失败", e))?;
    Ok(Json(
        rows.iter()
            .map(|r| ShotMeta {
                id: r.get("id"),
                name: r.get("name"),
                content_type: r.get("content_type"),
                size: r.get("size"),
                created_at: r.get("created_at"),
                created_by: r.get("created_by"),
            })
            .collect(),
    ))
}

pub(crate) async fn api_get_screenshot(State(state): State<ApiState>, Path((id, sid)): Path<(String, String)>) -> Result<impl IntoResponse, ApiError> {
    let module = module_uuid(&id)?;
    let sid: Uuid = sid.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这张图".to_string()))?;
    let row = sqlx::query("select content_type, bytes from module_screenshots where id = $1 and module_id = $2")
        .bind(sid)
        .bind(module)
        .fetch_optional(state.app.pool())
        .await
        .map_err(|e| internal("读取 UI 图失败", e))?
        .ok_or((StatusCode::NOT_FOUND, "没有这张图".to_string()))?;
    let content_type: String = row.get("content_type");
    let bytes: Vec<u8> = row.get("bytes");
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "private, max-age=300".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        bytes,
    ))
}

#[derive(Deserialize)]
pub(crate) struct NamedShot {
    #[serde(default)]
    name: String,
    data_b64: String,
}

#[derive(Deserialize)]
pub(crate) struct AddShots {
    screenshots: Vec<NamedShot>,
}

fn shot_name(raw: &str, index: usize) -> String {
    let name: String = raw.trim().chars().filter(|c| !c.is_control()).take(120).collect();
    if name.is_empty() { format!("截图 {}", index + 1) } else { name }
}

/// 给模块加图：一次最多 100 张，单张 ≤ 3MB、只收 PNG / JPEG；模块里合计最多 100 张、100MB；同一张图（内容相同）不会存两份。
pub(crate) async fn api_add_screenshots(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<AddShots>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let module = module_uuid(&id)?;
    let Json(input) = body.map_err(|_| bad("请求格式无效，或内容太大（图片合计不要超过 100MB）"))?;
    if input.screenshots.is_empty() {
        return Err(bad("请选择要上传的图片"));
    }
    if input.screenshots.len() > MAX_SHOTS {
        return Err(bad(format!("一次最多上传 {MAX_SHOTS} 张")));
    }
    let user = writer(&state, &headers).await?;
    let mut shots = Vec::new();
    for (i, s) in input.screenshots.iter().enumerate() {
        let shot = decode_shot(i, &ShotInput { data_b64: s.data_b64.clone() }).map_err(|e| api_error(e, "校验图片失败"))?;
        let sha = hex::encode(Sha256::digest(&shot.bytes));
        shots.push((shot_name(&s.name, i), shot, sha));
    }
    let pool = state.app.pool();
    let mut tx = pool.begin().await.map_err(|e| internal("上传 UI 图失败", e))?;
    // 锁住模块这一行：两个人同时上传时，张数和总大小的检查才不会各算各的
    let exists: Option<Uuid> = sqlx::query_scalar("select id from modules where id = $1 for update")
        .bind(module)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| internal("上传 UI 图失败", e))?;
    if exists.is_none() {
        return Err((StatusCode::NOT_FOUND, "没有这个模块".to_string()));
    }
    let (count, total): (i64, i64) = sqlx::query_as("select count(*), coalesce(sum(length(bytes)), 0)::bigint from module_screenshots where module_id = $1")
        .bind(module)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| internal("上传 UI 图失败", e))?;
    let existing: Vec<String> = sqlx::query_scalar("select sha256 from module_screenshots where module_id = $1")
        .bind(module)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| internal("上传 UI 图失败", e))?;
    let mut seen: std::collections::HashSet<String> = existing.into_iter().collect();
    let mut fresh = Vec::new();
    let mut duplicates = 0usize;
    for item in shots {
        if seen.insert(item.2.clone()) { fresh.push(item) } else { duplicates += 1 }
    }
    let new_bytes: usize = fresh.iter().map(|(_, s, _)| s.bytes.len()).sum();
    if count as usize + fresh.len() > MAX_SHOTS {
        return Err(bad(format!("这个模块最多 {MAX_SHOTS} 张 UI 图，现在已有 {count} 张，请先删掉一些再传")));
    }
    if total as usize + new_bytes > MAX_TOTAL_SHOT_BYTES {
        return Err(bad(format!("这个模块的 UI 图合计不能超过 {} MB，请先删掉一些或裁小一点", MAX_TOTAL_SHOT_BYTES / 1024 / 1024)));
    }
    for (name, shot, sha) in &fresh {
        sqlx::query("insert into module_screenshots (module_id, name, content_type, bytes, sha256, created_by) values ($1,$2,$3,$4,$5,$6)")
            .bind(module)
            .bind(name)
            .bind(shot.content_type)
            .bind(&shot.bytes)
            .bind(sha)
            .bind(user)
            .execute(&mut *tx)
            .await
            .map_err(|e| internal("上传 UI 图失败", e))?;
    }
    tx.commit().await.map_err(|e| internal("上传 UI 图失败", e))?;
    Ok(Json(serde_json::json!({ "added": fresh.len(), "duplicates": duplicates })))
}

pub(crate) async fn api_delete_screenshot(
    State(state): State<ApiState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let module = module_uuid(&id)?;
    let sid: Uuid = sid.parse().map_err(|_| (StatusCode::NOT_FOUND, "没有这张图".to_string()))?;
    writer(&state, &headers).await?;
    let done = sqlx::query("delete from module_screenshots where id = $1 and module_id = $2")
        .bind(sid)
        .bind(module)
        .execute(state.app.pool())
        .await
        .map_err(|e| internal("删除 UI 图失败", e))?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "没有这张图".to_string()));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- CSV

pub(crate) const CSV_HEADER: [&str; 9] = ["编号", "标题", "运行环境", "级别", "前置条件", "步骤", "预期", "复核状态", "最近结果"];
const MAX_IMPORT_ROWS: usize = 500;
const MAX_IMPORT_BYTES: usize = 1024 * 1024;

/// 运行环境：app / h5 / browser。库里目前存的是 platforms（android / ios / app / h5 / web / browser），没写的按 app。
pub(crate) fn run_env_of(platforms: &[String]) -> &'static str {
    let has = |names: &[&str]| platforms.iter().any(|p| names.contains(&p.to_lowercase().as_str()));
    if has(&["browser", "web", "pc"]) {
        "browser"
    } else if has(&["h5", "mweb", "m-web"]) {
        "h5"
    } else {
        "app"
    }
}

/// 导入时运行环境对应写进 platforms 的值。
fn platforms_of(env: &str) -> Vec<String> {
    vec![match env {
        "h5" => "h5",
        "browser" => "browser",
        _ => "android",
    }
    .to_string()]
}

fn review_text(status: &str) -> &'static str {
    match status {
        "draft" => "草稿",
        "reviewed_ok" => "评审通过",
        "needs_human" => "待人工确认",
        "approved" => "已批准",
        "rejected" => "已拒绝",
        _ => "未知",
    }
}

fn result_text(final_result: Option<&str>, blocked_reason: Option<&str>, has_run: bool) -> String {
    if blocked_reason == Some("skipped") {
        return "已跳过".into();
    }
    match (final_result, has_run) {
        (Some("passed"), _) => "通过".into(),
        (Some("failed"), _) => "失败".into(),
        (Some("uncertain"), _) => "待人工确认".into(),
        (Some("blocked"), _) => "受阻".into(),
        (Some(other), _) => other.to_string(),
        (None, true) => "进行中".into(),
        (None, false) => "未执行".into(),
    }
}

/// CSV 一个单元格：含逗号、引号、换行就加引号；以 = + @ 开头的前面补一个 '，免得 Excel 当公式执行。
fn csv_cell(value: &str) -> String {
    let guarded = if value.starts_with(['=', '+', '@', '\t', '\r']) { format!("'{value}") } else { value.to_string() };
    if guarded.contains([',', '"', '\n', '\r']) { format!("\"{}\"", guarded.replace('"', "\"\"")) } else { guarded }
}

pub(crate) fn csv_line(fields: &[String]) -> String {
    fields.iter().map(|f| csv_cell(f)).collect::<Vec<_>>().join(",")
}

/// 解析 CSV（RFC 4180：引号、`""` 转义、单元格里可以有换行；兼容 BOM 和 `\r\n`）。空行跳过。
pub(crate) fn parse_csv(text: &str) -> Result<Vec<Vec<String>>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let (mut row, mut cell, mut in_quotes, mut was_quoted) = (Vec::<String>::new(), String::new(), false, false);
    let mut chars = text.chars().peekable();
    let mut line = 1usize;
    let finish_row = |row: &mut Vec<String>, cell: &mut String, rows: &mut Vec<Vec<String>>, was_quoted: &mut bool| {
        row.push(std::mem::take(cell));
        *was_quoted = false;
        let blank = row.iter().all(|c| c.trim().is_empty());
        let done = std::mem::take(row);
        if !blank {
            rows.push(done);
        }
    };
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    cell.push('"');
                    chars.next();
                }
                '"' => in_quotes = false,
                '\n' => {
                    line += 1;
                    cell.push('\n');
                }
                _ => cell.push(c),
            }
            continue;
        }
        match c {
            '"' if cell.is_empty() && !was_quoted => {
                in_quotes = true;
                was_quoted = true;
            }
            ',' => {
                row.push(std::mem::take(&mut cell));
                was_quoted = false;
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line += 1;
                finish_row(&mut row, &mut cell, &mut rows, &mut was_quoted);
            }
            '\n' => {
                line += 1;
                finish_row(&mut row, &mut cell, &mut rows, &mut was_quoted);
            }
            _ => cell.push(c),
        }
    }
    if in_quotes {
        return Err(format!("第 {line} 行附近有一个引号没有闭合"));
    }
    if !cell.is_empty() || !row.is_empty() {
        finish_row(&mut row, &mut cell, &mut rows, &mut was_quoted);
    }
    Ok(rows)
}

/// 导出时补的防公式单引号，导入时去掉。
fn unguard(cell: &str) -> &str {
    match cell.strip_prefix('\'') {
        Some(rest) if rest.starts_with(['=', '+', '@', '\t', '\r']) => rest,
        _ => cell,
    }
}

fn lines_of(cell: &str) -> Vec<String> {
    unguard(cell).replace("\r\n", "\n").replace('\r', "\n").split('\n').map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

#[derive(Debug)]
pub(crate) struct CsvCase {
    pub code: String,
    pub title: String,
    pub env: &'static str,
    pub level: String,
    pub preconditions: Vec<String>,
    pub steps: Vec<String>,
    pub expected: Vec<String>,
}

/// 把 CSV 文本整个校验成用例；任何一行不合格就返回全部问题（带行号），一条都不导入。
pub(crate) fn parse_cases(text: &str) -> Result<Vec<CsvCase>, Vec<String>> {
    let rows = parse_csv(text).map_err(|e| vec![e])?;
    let mut rows = rows.into_iter();
    let Some(header) = rows.next() else { return Err(vec!["文件是空的：第一行要是表头（编号、标题、运行环境、级别、前置条件、步骤、预期）".into()]) };
    let column = |name: &str| header.iter().position(|h| h.trim() == name);
    let missing: Vec<&str> = ["编号", "标题", "步骤", "预期"].into_iter().filter(|n| column(n).is_none()).collect();
    if !missing.is_empty() {
        return Err(vec![format!("表头缺少这些列：{}。可以先「导出 CSV」拿到模板", missing.join("、"))]);
    }
    let (c_code, c_title, c_env, c_level, c_pre, c_steps, c_exp) =
        (column("编号"), column("标题"), column("运行环境"), column("级别"), column("前置条件"), column("步骤"), column("预期"));
    fn get(row: &[String], col: Option<usize>) -> &str {
        col.and_then(|i| row.get(i)).map(|s| s.as_str()).unwrap_or("")
    }
    let body: Vec<Vec<String>> = rows.collect();
    if body.is_empty() {
        return Err(vec!["文件里只有表头，没有用例".into()]);
    }
    if body.len() > MAX_IMPORT_ROWS {
        return Err(vec![format!("一次最多导入 {MAX_IMPORT_ROWS} 条用例，这个文件有 {} 条", body.len())]);
    }
    let (mut cases, mut errors) = (Vec::new(), Vec::new());
    let mut seen = std::collections::HashSet::new();
    for (i, row) in body.iter().enumerate() {
        let line = i + 2; // 表头是第 1 行
        let code = get(row, c_code).trim().to_string();
        let title = unguard(get(row, c_title)).trim().to_string();
        let mut problems = Vec::new();
        if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) || code.len() > 40 {
            problems.push("编号只能用字母、数字、- _ .，不超过 40 位，且不能为空".to_string());
        } else if !seen.insert(code.clone()) {
            problems.push(format!("编号 {code} 在文件里重复了"));
        }
        if title.is_empty() || title.chars().count() > 200 {
            problems.push("标题不能为空，也不超过 200 个字".into());
        }
        let env_text = get(row, c_env).trim().to_lowercase();
        let env = match env_text.as_str() {
            "" | "app" => "app",
            "h5" => "h5",
            "browser" | "浏览器" => "browser",
            _ => {
                problems.push("运行环境只能是 app、h5、browser（或留空按 app）".into());
                "app"
            }
        };
        let level = unguard(get(row, c_level)).trim().to_string();
        if level.chars().count() > 40 {
            problems.push("级别不超过 40 个字".into());
        }
        let (preconditions, steps, expected) = (lines_of(get(row, c_pre)), lines_of(get(row, c_steps)), lines_of(get(row, c_exp)));
        if steps.is_empty() {
            problems.push("步骤不能为空（一行一步，单元格里换行）".into());
        }
        if expected.is_empty() {
            problems.push("预期不能为空（一行一条，评审员要逐条判定）".into());
        }
        // 规则 2：明文密码一律拒绝，错误里不回显值
        let all = [title.as_str(), &preconditions.join("\n"), &steps.join("\n"), &expected.join("\n")].join("\n");
        if plain_password_line(&all).is_some() {
            problems.push("里面有明文密码：账号密码只能写成 {{account.xxx}} 占位符（值未显示）".into());
        }
        if problems.is_empty() {
            cases.push(CsvCase { code, title, env, level, preconditions, steps, expected });
        } else {
            errors.extend(problems.into_iter().map(|p| format!("第 {line} 行：{p}")));
        }
    }
    if errors.is_empty() { Ok(cases) } else { Err(errors) }
}

pub(crate) async fn api_cases_csv(State(state): State<ApiState>, Path(id): Path<String>) -> Result<impl IntoResponse, ApiError> {
    let module = module_uuid(&id)?;
    let pool = state.app.pool();
    require_module(pool, module).await?;
    let rows = sqlx::query(
        "select c.code, c.title, c.level, c.platforms, c.preconditions, c.steps, c.expected, c.review_status,
                r.id as run_id, r.final_result, r.blocked_reason
         from test_cases c
         left join lateral (select * from runs x where x.case_id = c.id order by x.seq desc limit 1) r on true
         where c.module_id = $1 order by c.code",
    )
    .bind(module)
    .fetch_all(pool)
    .await
    .map_err(|e| internal("导出用例失败", e))?;
    let strings = |v: serde_json::Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default() };
    let mut out = String::from('\u{feff}');
    out.push_str(&csv_line(&CSV_HEADER.iter().map(|s| s.to_string()).collect::<Vec<_>>()));
    out.push_str("\r\n");
    for r in &rows {
        let platforms: Vec<String> = r.get("platforms");
        let expected: Vec<String> = r.get::<serde_json::Value, _>("expected").as_array().map(|a| a.iter().filter_map(|e| e["text"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
        let final_result: Option<String> = r.get("final_result");
        let line = [
            r.get::<String, _>("code"),
            r.get::<String, _>("title"),
            run_env_of(&platforms).to_string(),
            r.get::<String, _>("level"),
            strings(r.get("preconditions")).join("\n"),
            strings(r.get("steps")).join("\n"),
            expected.join("\n"),
            review_text(&r.get::<String, _>("review_status")).to_string(),
            result_text(final_result.as_deref(), r.get::<Option<String>, _>("blocked_reason").as_deref(), r.get::<Option<Uuid>, _>("run_id").is_some()),
        ];
        out.push_str(&csv_line(&line));
        out.push_str("\r\n");
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"cases.csv\"".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        out,
    ))
}

#[derive(Deserialize)]
pub(crate) struct ImportCsv {
    csv: String,
}

/// 导入 CSV：整体校验后写进这个模块；已有的编号（本模块的）按内容更新，别的模块的编号不能占用。
/// 导入的用例是人给的，直接是「已批准」；复核状态和最近结果两列只用于导出，导入时忽略。
pub(crate) async fn api_import_cases(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<ImportCsv>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let module = module_uuid(&id)?;
    let Json(input) = body.map_err(|_| bad("请求格式无效，或文件太大（不超过 1MB）"))?;
    if input.csv.len() > MAX_IMPORT_BYTES {
        return Err(bad("CSV 文件太大（不超过 1MB）"));
    }
    let cases = parse_cases(&input.csv).map_err(|errors| {
        let shown: Vec<&String> = errors.iter().take(10).collect();
        let more = if errors.len() > shown.len() { format!("（还有 {} 处问题没列出）", errors.len() - shown.len()) } else { String::new() };
        bad(format!("CSV 里有问题，整个文件都没有导入：{}{more}", shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("；")))
    })?;
    writer(&state, &headers).await?;
    let pool = state.app.pool();
    let mut tx = pool.begin().await.map_err(|e| internal("导入用例失败", e))?;
    let project_id: Uuid = sqlx::query_scalar("select project_id from modules where id = $1 for update")
        .bind(module)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| internal("导入用例失败", e))?
        .ok_or((StatusCode::NOT_FOUND, "没有这个模块".to_string()))?;
    let (mut inserted, mut updated, mut unchanged) = (0u32, 0u32, 0u32);
    for c in &cases {
        let owner: Option<Option<Uuid>> = sqlx::query_scalar("select module_id from test_cases where project_id = $1 and code = $2")
            .bind(project_id)
            .bind(&c.code)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| internal("导入用例失败", e))?;
        if let Some(other) = owner {
            if other != Some(module) {
                return Err(bad(format!("编号 {} 已经被别的模块（或不属于任何模块的老用例）占用了，请换一个编号", c.code)));
            }
        }
        let expected: Vec<serde_json::Value> = c.expected.iter().enumerate().map(|(i, t)| serde_json::json!({ "idx": i + 1, "text": t })).collect();
        let row = sqlx::query(
            "insert into test_cases (project_id, code, title, level, platforms, preconditions, steps, expected, should_fail, status, module_id, origin, review_status)
             values ($1,$2,$3,$4,$5,$6,$7,$8,$9,'active',$10,'imported','approved')
             on conflict (project_id, code) do update set
               title = excluded.title, level = excluded.level, platforms = excluded.platforms,
               preconditions = excluded.preconditions, steps = excluded.steps, expected = excluded.expected,
               should_fail = excluded.should_fail, version = test_cases.version + 1, updated_at = now()
             where test_cases.module_id = excluded.module_id
               and (test_cases.title, test_cases.level, test_cases.platforms, test_cases.preconditions, test_cases.steps, test_cases.expected, test_cases.should_fail)
                   is distinct from
                   (excluded.title, excluded.level, excluded.platforms, excluded.preconditions, excluded.steps, excluded.expected, excluded.should_fail)
             returning (xmax = 0) as inserted",
        )
        .bind(project_id)
        .bind(&c.code)
        .bind(&c.title)
        .bind(&c.level)
        .bind(platforms_of(c.env))
        .bind(serde_json::json!(c.preconditions))
        .bind(serde_json::json!(c.steps))
        .bind(serde_json::Value::Array(expected))
        .bind(c.level == crate::cases::NEGATIVE_LEVEL)
        .bind(module)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| internal("导入用例失败", e))?;
        match row {
            Some(r) if r.get::<bool, _>("inserted") => inserted += 1,
            Some(_) => updated += 1,
            None => unchanged += 1,
        }
    }
    tx.commit().await.map_err(|e| internal("导入用例失败", e))?;
    Ok(Json(serde_json::json!({ "inserted": inserted, "updated": updated, "unchanged": unchanged })))
}

// ---------------------------------------------------------------- 录屏

#[derive(Serialize)]
struct CaseRecording {
    seq: i32,
    url: String,
    duration_ms: Option<i64>,
    start_epoch_ms: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct CaseRecordings {
    code: String,
    title: String,
    run_id: Uuid,
    final_result: Option<String>,
    recordings: Vec<CaseRecording>,
    /// 签名地址的过期时间（Unix 秒），过期了刷新页面重新取
    expires_at: i64,
}

/// 这个模块各用例**最近一次**运行的录屏（签名地址，短期有效）。没跑过的用例不出现。
pub(crate) async fn api_module_recordings(State(state): State<ApiState>, Path(id): Path<String>) -> Result<Json<Vec<CaseRecordings>>, ApiError> {
    let module = module_uuid(&id)?;
    let pool = state.app.pool();
    require_module(pool, module).await?;
    let rows = sqlx::query(
        "select c.code, c.title, r.id as run_id, r.final_result
         from test_cases c
         join lateral (select * from runs x where x.case_id = c.id order by x.seq desc limit 1) r on true
         where c.module_id = $1 order by c.code",
    )
    .bind(module)
    .fetch_all(pool)
    .await
    .map_err(|e| internal("查询录屏失败", e))?;
    let expires_at = Utc::now().timestamp() + crate::evidence_sign::TTL_SECS;
    let mut conn = pool.acquire().await.map_err(|e| internal("查询录屏失败", e))?;
    let mut out = Vec::new();
    for r in &rows {
        let run_id: Uuid = r.get("run_id");
        let recordings = crate::files::recordings(&mut conn, run_id).await.map_err(|e| internal("查询录屏失败", e))?;
        out.push(CaseRecordings {
            code: r.get("code"),
            title: r.get("title"),
            run_id,
            final_result: r.get("final_result"),
            recordings: recordings
                .into_iter()
                .map(|rec| CaseRecording { seq: rec.seq, url: state.evidence_signer.url(run_id, &rec.name, expires_at), duration_ms: rec.duration_ms, start_epoch_ms: rec.start_epoch_ms })
                .collect(),
            expires_at,
        });
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[&[&str]]) -> String {
        rows.iter().map(|r| csv_line(&r.iter().map(|s| s.to_string()).collect::<Vec<_>>())).collect::<Vec<_>>().join("\r\n")
    }

    #[test]
    fn csv_往返_引号逗号换行都不丢() {
        let rows = [vec!["a,b".to_string(), "说\"你好\"".to_string(), "第一行\n第二行".to_string(), "".to_string()]];
        let line = csv_line(&rows[0]);
        assert_eq!(parse_csv(&format!("\u{feff}{line}\r\n")).unwrap(), rows.to_vec());
        // 空行跳过；LF 和 CRLF 都行
        assert_eq!(parse_csv("a,b\n\nc,d\r\n").unwrap().len(), 2);
    }

    #[test]
    fn csv_公式开头加单引号_导入时去掉() {
        assert_eq!(csv_cell("=1+1"), "'=1+1");
        assert_eq!(unguard("'=1+1"), "=1+1");
        assert_eq!(unguard("'普通单引号"), "'普通单引号");
    }

    #[test]
    fn csv_引号没闭合要报错() {
        assert!(parse_csv("a,\"b\nc").unwrap_err().contains("没有闭合"));
    }

    #[test]
    fn 导入整体校验_缺列_重复_非法环境_空步骤_明文密码都带行号() {
        let head: &[&str] = &["编号", "标题", "运行环境", "步骤", "预期"];
        // 缺必需列
        assert!(parse_cases(&text(&[&["编号", "标题"], &["T1", "x"]])).unwrap_err()[0].contains("缺少这些列"));
        // 多种问题一起报，行号从表头算起
        let bad = text(&[
            head,
            &["T1", "好用例", "app", "点一下", "出现页面"],
            &["T1", "编号重复", "app", "点一下", "出现页面"],
            &["T3", "环境不对", "ios", "点一下", "出现页面"],
            &["T4", "没有步骤", "", "", "出现页面"],
            &["T5", "有密码", "", "输入密码：hunter2", "登录成功"],
        ]);
        let errors = parse_cases(&bad).unwrap_err().join("|");
        for want in ["第 3 行：编号 T1 在文件里重复", "第 4 行：运行环境只能是", "第 5 行：步骤不能为空", "第 6 行：里面有明文密码"] {
            assert!(errors.contains(want), "{want} 不在 {errors}");
        }
        assert!(!errors.contains("hunter2"), "错误里不能回显密码");
        // 全部合格：环境留空按 app，一个单元格里换行就是多步
        let ok = text(&[head, &["T1", "好用例", "", "点入口\n点充值", "出现页面\n显示金额"], &["T2", "h5 用例", "h5", "打开链接", "能看到"]]);
        let cases = parse_cases(&ok).unwrap();
        assert_eq!((cases[0].env, cases[0].steps.len(), cases[0].expected.len(), cases[1].env), ("app", 2, 2, "h5"));
    }

    #[test]
    fn 运行环境按_platforms_推导() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(run_env_of(&p(&["android"])), "app");
        assert_eq!(run_env_of(&p(&[])), "app");
        assert_eq!(run_env_of(&p(&["h5"])), "h5");
        assert_eq!(run_env_of(&p(&["web"])), "browser");
        assert_eq!(platforms_of("browser"), p(&["browser"]));
    }
}
