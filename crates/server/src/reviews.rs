use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Request, State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::AppState;
use crate::api::{ApiState, public_text};

#[path = "reviews_export.rs"]
mod export_api;
pub(crate) use export_api::Redactor;
pub(crate) use export_api::export;

const TOKEN_HEADER: &str = "x-review-token";
const WINDOW: Duration = Duration::from_secs(60);
const FAILURE_LIMIT: usize = 10;
const VERDICTS: [&str; 3] = ["passed", "failed", "uncertain"];

struct ReviewToken {
    value: String,
    digest: [u8; 32],
}

impl ReviewToken {
    fn matches(&self, provided: &[u8]) -> bool {
        let digest: [u8; 32] = Sha256::digest(provided).into();
        bool::from(digest.ct_eq(&self.digest))
    }
}

pub(crate) struct ReviewAuth {
    token: Option<ReviewToken>,
    failures: Mutex<Failures>,
}

impl ReviewAuth {
    pub(crate) fn from_env() -> Self {
        let auth = Self::new(std::env::var("PLATFORM_REVIEW_TOKEN").ok());
        if auth.token.is_none() {
            tracing::warn!("创建任务、取消执行和审核提交已关闭：请设置至少 16 个字符的 PLATFORM_REVIEW_TOKEN");
        }
        auth
    }

    pub(crate) fn new(value: Option<String>) -> Self {
        let token = value
            .filter(|token| token.chars().count() >= 16)
            .map(|value| ReviewToken {
                digest: Sha256::digest(value.as_bytes()).into(),
                value,
            });
        Self {
            token,
            failures: Mutex::default(),
        }
    }

    fn token_text(&self) -> Option<&str> {
        self.token.as_ref().map(|token| token.value.as_str())
    }

    fn check(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let token = self.token.as_ref().ok_or(StatusCode::FORBIDDEN)?;
        let mut values = headers.get_all(TOKEN_HEADER).iter();
        let provided = values.next();
        let valid = provided
            .is_some_and(|value| values.next().is_none() && token.matches(value.as_bytes()));
        let mut failures = self.failures.lock().map_err(|_| StatusCode::FORBIDDEN)?;
        failures.check(Instant::now(), provided.map(|_| valid))
    }
}

#[derive(Default)]
struct Failures {
    attempts: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

impl Failures {
    fn check(&mut self, now: Instant, matched: Option<bool>) -> Result<(), StatusCode> {
        if self.blocked_until.is_some_and(|until| now < until) {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        self.blocked_until = None;
        if matched == Some(true) {
            return Ok(());
        }
        if matched == Some(false) {
            while self
                .attempts
                .front()
                .is_some_and(|time| now.duration_since(*time) >= WINDOW)
            {
                self.attempts.pop_front();
            }
            if self.attempts.len() == FAILURE_LIMIT {
                self.attempts.clear();
                self.blocked_until = Some(now + WINDOW);
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
            self.attempts.push_back(now);
        }
        Err(StatusCode::FORBIDDEN)
    }
}

pub(crate) async fn authorize(
    State(app): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    app.0.review_auth.check(request.headers())?;
    request.headers_mut().remove(TOKEN_HEADER);
    Ok(next.run(request).await)
}

type ApiError = (StatusCode, &'static str);

fn db_error(_: sqlx::Error) -> ApiError {
    tracing::error!("审核数据的数据库操作失败");
    (StatusCode::INTERNAL_SERVER_ERROR, "无法处理审核数据")
}

fn bad_request(message: &'static str) -> ApiError {
    (StatusCode::BAD_REQUEST, message)
}

const RUN_NOT_FOUND: ApiError = (StatusCode::NOT_FOUND, "没有这个 run");

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mark {
    Correct,
    Passed,
    Failed,
}

impl Mark {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Correct => "correct",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct NewReview {
    item_idx: Option<i32>,
    mark: Mark,
    note: Option<String>,
    reviewer: String,
}

impl NewReview {
    fn validate(&self) -> Result<(), ApiError> {
        if !(1..=32).contains(&self.reviewer.chars().count())
            || self.reviewer.trim().is_empty()
            || self.reviewer.chars().any(char::is_control)
        {
            return Err(bad_request("审核人姓名须为 1～32 个字符，且不能含控制字符"));
        }
        if self
            .note
            .as_ref()
            .is_some_and(|note| note.chars().count() > 2000 || note.contains('\0'))
        {
            return Err(bad_request("审核备注最多 2000 个字符，且不能含空字符"));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, sqlx::FromRow)]
pub(crate) struct Review {
    id: Uuid,
    run_id: Uuid,
    item_idx: Option<i32>,
    mark: String,
    judge_verdict: String,
    note: Option<String>,
    reviewer: String,
    created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub(crate) struct RunReviews {
    history: Vec<Review>,
    current: Vec<Review>,
}

pub(crate) async fn list(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<RunReviews>, ApiError> {
    let id: Uuid = id.parse().map_err(|_| RUN_NOT_FOUND)?;
    let mut tx = state.app.pool().begin().await.map_err(db_error)?;
    sqlx::query("set transaction isolation level repeatable read read only")
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    let exists: bool = sqlx::query_scalar("select exists(select 1 from runs where id = $1)")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
    if !exists {
        return Err(RUN_NOT_FOUND);
    }
    let history: Vec<Review> =
        sqlx::query_as("select * from reviews where run_id = $1 order by created_at desc, id desc")
            .bind(id)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    let mut seen = HashSet::new();
    let current = history
        .iter()
        .filter(|row| seen.insert(row.item_idx))
        .cloned()
        .collect();
    Ok(Json(RunReviews { history, current }))
}

pub(crate) async fn submit(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Result<Json<NewReview>, JsonRejection>,
) -> Result<(StatusCode, Json<Review>), ApiError> {
    let id: Uuid = id.parse().map_err(|_| RUN_NOT_FOUND)?;
    let Json(mut review) = body.map_err(|_| bad_request("审核参数格式无效"))?;
    review.validate()?;
    let mut tx = state.app.pool().begin().await.map_err(db_error)?;
    let (status, final_result): (String, Option<String>) =
        sqlx::query_as("select status, final_result from runs where id = $1 for update")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_error)?
            .ok_or(RUN_NOT_FOUND)?;
    let final_result = final_result
        .filter(|_| status == "finished")
        .ok_or((StatusCode::CONFLICT, "只有已结束的 run 可以审核"))?;
    let verdict = match review.item_idx {
        Some(idx) => {
            sqlx::query_scalar("select verdict from judge_items where run_id = $1 and idx = $2")
                .bind(id)
                .bind(idx)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_error)?
                .ok_or_else(|| bad_request("没有这个 Judge 判定条目"))?
        }
        None => final_result,
    };
    let redactor = Redactor::load(&state.app, &mut tx).await?;
    review.reviewer = redactor.text(review.reviewer.trim());
    review.note = review.note.map(|note| redactor.text(&note));
    review.validate()?;
    let row = sqlx::query_as(
        "insert into reviews (run_id, item_idx, mark, judge_verdict, note, reviewer)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(id)
    .bind(review.item_idx)
    .bind(review.mark.as_str())
    .bind(verdict)
    .bind(review.note)
    .bind(review.reviewer)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    Ok((StatusCode::CREATED, Json(row)))
}

fn human_verdict<'a>(mark: &'a str, judge_verdict: &'a str) -> &'a str {
    if mark == "correct" {
        judge_verdict
    } else {
        mark
    }
}

#[derive(Default, Serialize)]
struct Counts {
    reviewed: u64,
    correct: u64,
    judge_correct_rate: Option<f64>,
}

impl Counts {
    fn add(&mut self, mark: &str) {
        self.reviewed += 1;
        self.correct += u64::from(mark == "correct");
        self.judge_correct_rate = Some(self.correct as f64 / self.reviewed as f64);
    }
}

#[derive(Serialize)]
pub(crate) struct Stats {
    #[serde(flatten)]
    total: Counts,
    confusion: BTreeMap<&'static str, BTreeMap<&'static str, u64>>,
    by_case: BTreeMap<String, Counts>,
}

impl Stats {
    fn calculate(rows: Vec<(String, String, String)>) -> Self {
        let mut stats = Self {
            total: Counts::default(),
            confusion: VERDICTS
                .into_iter()
                .map(|judge| {
                    (
                        judge,
                        VERDICTS.into_iter().map(|human| (human, 0)).collect(),
                    )
                })
                .collect(),
            by_case: BTreeMap::new(),
        };
        for (case_code, mark, judge) in rows {
            stats.total.add(&mark);
            stats.by_case.entry(case_code).or_default().add(&mark);
            if let Some(cell) = stats
                .confusion
                .get_mut(judge.as_str())
                .and_then(|row| row.get_mut(human_verdict(&mark, &judge)))
            {
                *cell += 1;
            }
        }
        stats
    }
}

pub(crate) async fn stats(State(state): State<ApiState>) -> Result<Json<Stats>, ApiError> {
    let mut rows: Vec<(String, String, String)> = sqlx::query_as(
        "select r.case_spec->>'code', c.mark, c.judge_verdict
         from current_reviews c join runs r on r.id = c.run_id",
    )
    .fetch_all(state.app.pool())
    .await
    .map_err(db_error)?;
    if let Some(token) = state.app.0.review_auth.token_text() {
        for (code, _, _) in &mut rows {
            *code = public_text(&code.replace(token, "（已脱敏）"));
        }
    }
    Ok(Json(Stats::calculate(rows)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 审核口令以定长摘要常量时间比较且不接受前后缀() {
        let auth = ReviewAuth::new(Some("0123456789abcdef".into()));
        let token = auth.token.unwrap();
        assert_eq!(token.digest.len(), 32);
        assert!(token.matches(b"0123456789abcdef"));
        for other in [
            "X123456789abcdef",
            "0123456789abcdeX",
            "0123456789abcde",
            "0123456789abcdefX",
            "",
        ] {
            assert!(!token.matches(other.as_bytes()));
        }
    }

    #[test]
    fn 审核口令缺失或短于十六字符时禁用提交() {
        for configured in [
            None,
            Some(String::new()),
            Some("密".repeat(15)),
            Some("0123456789abcde".into()),
        ] {
            let auth = ReviewAuth::new(configured);
            assert!(auth.token.is_none());
            let mut headers = HeaderMap::new();
            headers.insert(TOKEN_HEADER, "0123456789abcde".parse().unwrap());
            assert_eq!(auth.check(&headers), Err(StatusCode::FORBIDDEN));
        }
    }

    #[test]
    fn 审核口令错误按滑动窗口累计且锁定不会被请求延长() {
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let mut failures = Failures::default();
        let wrong = Some(false);
        assert_eq!(failures.check(at(0), wrong), Err(StatusCode::FORBIDDEN));
        for _ in 0..9 {
            assert_eq!(failures.check(at(45), wrong), Err(StatusCode::FORBIDDEN));
        }
        assert_eq!(failures.check(at(59), None), Err(StatusCode::FORBIDDEN));
        assert_eq!(failures.check(at(60), wrong), Err(StatusCode::FORBIDDEN));
        assert_eq!(failures.check(at(60), Some(true)), Ok(()));
        assert_eq!(
            failures.check(at(60), wrong),
            Err(StatusCode::TOO_MANY_REQUESTS)
        );
        for attempt in [wrong, None, Some(true)] {
            assert_eq!(
                failures.check(at(119), attempt),
                Err(StatusCode::TOO_MANY_REQUESTS)
            );
        }
        assert_eq!(failures.check(at(120), Some(true)), Ok(()));
        assert_eq!(failures.check(at(120), wrong), Err(StatusCode::FORBIDDEN));
    }

    #[test]
    fn 审核校验标注和编号以及姓名备注的字符边界() {
        for mark in ["correct", "passed", "failed"] {
            let input = json!({"item_idx": 1, "mark": mark, "reviewer": "😀".repeat(32), "note": "字".repeat(2000)});
            let review: NewReview = serde_json::from_value(input).unwrap();
            assert!(review.validate().is_ok());
            assert_eq!(review.mark.as_str(), mark);
        }
        let whole: NewReview =
            serde_json::from_value(json!({"mark":"correct","reviewer":"张三"})).unwrap();
        assert!(whole.item_idx.is_none());
        assert!(whole.note.is_none());
        let base = json!({"mark":"correct", "reviewer":"审核员"});
        for (field, invalid) in [
            ("reviewer", json!("")),
            ("reviewer", json!("　 ")),
            ("reviewer", json!("审核\n员")),
            ("reviewer", json!("审核\u{007f}")),
            ("reviewer", json!("😀".repeat(33))),
            ("reviewer", json!(format!("{} ", "字".repeat(32)))),
            ("note", json!("字".repeat(2001))),
            ("note", json!("空\0字符")),
        ] {
            let mut input = base.clone();
            input[field] = invalid;
            let review: NewReview = serde_json::from_value(input).unwrap();
            assert_eq!(review.validate().unwrap_err().0, StatusCode::BAD_REQUEST);
        }
        for (field, invalid) in [
            ("mark", json!("uncertain")),
            ("mark", json!(false)),
            ("item_idx", json!(1.5)),
            ("item_idx", json!("1")),
            ("item_idx", json!(i64::MAX)),
        ] {
            let mut input = base.clone();
            input[field] = invalid;
            assert!(serde_json::from_value::<NewReview>(input).is_err());
        }
    }

    #[test]
    fn 审核统计按标注而非相同判定计正确且填满混淆矩阵() {
        let stats = Stats::calculate(
            [
                ("TC-04", "correct", "passed"),
                ("TC-04", "failed", "passed"),
                ("TC-04", "passed", "passed"),
                ("TC-05", "correct", "uncertain"),
                ("TC-05", "passed", "failed"),
            ]
            .map(|(code, mark, judge)| (code.into(), mark.into(), judge.into()))
            .to_vec(),
        );
        assert_eq!(stats.total.reviewed, 5);
        assert_eq!(stats.total.judge_correct_rate, Some(0.4));
        assert_eq!(stats.by_case["TC-05"].judge_correct_rate, Some(0.5));
        assert_eq!(stats.confusion["passed"]["passed"], 2);
        assert_eq!(stats.confusion["passed"]["failed"], 1);
        assert_eq!(stats.confusion["failed"]["passed"], 1);
        assert_eq!(stats.confusion["uncertain"]["uncertain"], 1);
        assert_eq!(stats.confusion["uncertain"]["failed"], 0);
        let empty = Stats::calculate(vec![]);
        assert_eq!(empty.total.reviewed, 0);
        assert!(empty.total.judge_correct_rate.is_none());
        assert!(empty.by_case.is_empty());
    }
}
