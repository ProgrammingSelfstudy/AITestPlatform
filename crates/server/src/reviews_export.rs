use std::collections::{BTreeSet, HashSet};

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use protocol::CaseExpected;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{ApiError, Review, db_error, human_verdict};
use crate::api::{ApiState, expand_data, public_text};
use crate::{AppState, accounts, files};

const MASK: &str = "（已脱敏）";
const REDACTION_UNAVAILABLE: ApiError = (StatusCode::SERVICE_UNAVAILABLE, "无法安全脱敏审核数据");

pub(crate) struct Redactor(Vec<String>);

impl Redactor {
    fn new(mut secrets: Vec<String>) -> Self {
        secrets.retain(|value| !value.is_empty());
        secrets.sort_unstable_by(|left, right| {
            right.len().cmp(&left.len()).then_with(|| left.cmp(right))
        });
        secrets.dedup();
        Self(secrets)
    }

    pub(crate) async fn load(app: &AppState, conn: &mut PgConnection) -> Result<Self, ApiError> {
        let mut secrets: Vec<String> = sqlx::query_scalar(
            "select serial from devices union select device_serial from test_accounts where device_serial is not null",
        )
        .fetch_all(&mut *conn).await.map_err(db_error)?;
        secrets.push(app.0.cfg.node_token.clone());
        secrets.extend(app.0.review_auth.token_text().map(str::to_owned));
        let accounts: Vec<(Uuid, String, Vec<u8>, Vec<u8>)> =
            sqlx::query_as("select id, nickname, username_enc, password_enc from test_accounts")
                .fetch_all(&mut *conn)
                .await
                .map_err(db_error)?;
        for (id, nickname, username_enc, password_enc) in accounts {
            let key = app
                .0
                .accounts
                .as_ref()
                .map(|pool| &pool.key)
                .ok_or(REDACTION_UNAVAILABLE)?;
            let username =
                accounts::decrypt(key, id, &username_enc).map_err(|_| REDACTION_UNAVAILABLE)?;
            let password =
                accounts::decrypt(key, id, &password_enc).map_err(|_| REDACTION_UNAVAILABLE)?;
            if let Some((local, _)) = username.split_once('@') {
                secrets.push(local.to_owned());
            }
            secrets.extend([id.to_string(), nickname, username, password]);
        }
        Ok(Self::new(secrets))
    }

    pub(crate) fn text(&self, text: &str) -> String {
        let mut text = public_text(text);
        for secret in &self.0 {
            if text.contains(secret) {
                text = text.replace(secret, MASK);
            }
        }
        text
    }

    fn export_text(&self, text: &str) -> String {
        let text = self.text(text);
        let boundary = |character: char| {
            character.is_whitespace()
                || "\"'`()[]{}<>,;，。；：！？（）【】《》‘’“”".contains(character)
        };
        let mut output = String::with_capacity(text.len());
        for span in text.split_inclusive(boundary) {
            let word = span.trim_end_matches(boundary);
            if word.contains(['/', '\\']) && !word.trim_matches(['/', '\\']).is_empty() {
                output.push_str(MASK);
            } else {
                output.push_str(word);
            }
            output.push_str(&span[word.len()..]);
        }
        output
    }
}

#[derive(sqlx::FromRow)]
struct ExportSource {
    #[sqlx(flatten)]
    review: Review,
    case_code: String,
    case_version: i32,
    case_data: Option<Value>,
    #[sqlx(json)]
    expected: Vec<CaseExpected>,
}

#[derive(sqlx::FromRow)]
struct EvidenceItem {
    idx: i32,
    expected_text: String,
    evidence: String,
    file_refs: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct EvidenceStep {
    idx: i32,
    at_ms: Option<i64>,
    file_ref: Option<String>,
}

struct RunEvidence {
    items: Vec<EvidenceItem>,
    steps: Vec<EvidenceStep>,
    recordings: Vec<files::RecordingView>,
}

impl RunEvidence {
    async fn load(conn: &mut PgConnection, run_id: Uuid) -> Result<Self, sqlx::Error> {
        let items = sqlx::query_as(
            "select idx, expected_text, evidence, file_refs from judge_items where run_id = $1 order by idx",
        )
        .bind(run_id).fetch_all(&mut *conn).await?;
        let steps = sqlx::query_as(
            "select idx, at_ms, file_ref from run_steps where run_id = $1 order by idx",
        )
        .bind(run_id)
        .fetch_all(&mut *conn)
        .await?;
        let recordings = files::recordings(conn, run_id).await?;
        Ok(Self {
            items,
            steps,
            recordings,
        })
    }
}

#[derive(Serialize)]
struct RecordingPosition {
    step_idx: i32,
    seq: i32,
    offset_seconds: f64,
}

#[derive(Serialize)]
struct ExportRow<'a> {
    case_code: String,
    case_version: i32,
    item_idx: Option<i32>,
    expected_text: String,
    judge_verdict: &'a str,
    evidence: String,
    file_refs: Vec<&'a str>,
    mark: &'a str,
    human_verdict: &'a str,
    note: Option<String>,
    reviewer: String,
    created_at: DateTime<Utc>,
    has_recording: bool,
    recording_position: Option<RecordingPosition>,
}

fn filename(reference: &str) -> Option<&str> {
    files::valid_reference(reference).then(|| reference.strip_prefix("shots/").unwrap_or(reference))
}

fn bounds(segment: &files::RecordingView) -> Option<(i64, i64)> {
    let start = segment.start_epoch_ms?;
    let duration = segment.duration_ms.filter(|value| *value > 0)?;
    Some((start, start.checked_add(duration)?))
}

fn position(recordings: &[files::RecordingView], step: &EvidenceStep) -> Option<RecordingPosition> {
    let at_ms = step.at_ms?;
    let (seq, offset) = recordings
        .iter()
        .find_map(|segment| {
            let (start, end) = bounds(segment)?;
            (start..end)
                .contains(&at_ms)
                .then(|| (segment.seq, at_ms - start))
        })
        .or_else(|| {
            recordings.windows(2).find_map(|pair| {
                let (_, previous_end) = bounds(&pair[0])?;
                let (next_start, _) = bounds(&pair[1])?;
                (previous_end..next_start)
                    .contains(&at_ms)
                    .then_some((pair[1].seq, 0))
            })
        })?;
    Some(RecordingPosition {
        step_idx: step.idx,
        seq,
        offset_seconds: offset as f64 / 1000.0,
    })
}

fn export_row<'a>(
    source: &'a ExportSource,
    context: &'a RunEvidence,
    redact: &Redactor,
) -> ExportRow<'a> {
    let review = &source.review;
    let items = || {
        context
            .items
            .iter()
            .filter(|item| review.item_idx.is_none_or(|idx| idx == item.idx))
    };
    let case_data = source.case_data.as_ref().unwrap_or(&Value::Null);
    let expected = |text: &str| redact.export_text(&expand_data(text, case_data));
    let expected_text = if review.item_idx.is_some() {
        items()
            .next()
            .map_or_else(String::new, |item| expected(&item.expected_text))
    } else {
        source
            .expected
            .iter()
            .map(|item| expected(&item.text))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let evidence = items()
        .map(|item| redact.export_text(&item.evidence))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let references = || items().flat_map(|item| item.file_refs.iter().map(String::as_str));
    let names: BTreeSet<&str> = references().filter_map(filename).collect();
    let cited_steps: HashSet<i32> = references()
        .filter(|reference| files::valid_judge_reference(reference))
        .filter_map(|reference| reference.strip_prefix("step:")?.parse().ok())
        .collect();
    let recording_position = context.steps.iter().find_map(|step| {
        let after = step.file_ref.as_deref().and_then(filename);
        let number = after
            .and_then(|name| name.split_once('_'))
            .map(|(prefix, _)| prefix.to_owned())
            .unwrap_or_else(|| format!("{:02}", step.idx));
        let cited = cited_steps.contains(&step.idx)
            || after.is_some_and(|name| names.contains(name))
            || names.contains(format!("{number}_observe.png").as_str())
            || names.contains(format!("{number}_after.png").as_str());
        cited.then(|| position(&context.recordings, step)).flatten()
    });
    ExportRow {
        case_code: redact.export_text(&source.case_code),
        case_version: source.case_version,
        item_idx: review.item_idx,
        expected_text,
        judge_verdict: &review.judge_verdict,
        evidence,
        file_refs: names
            .into_iter()
            .filter(|name| redact.text(name) == *name)
            .collect(),
        mark: &review.mark,
        human_verdict: human_verdict(&review.mark, &review.judge_verdict),
        note: review.note.as_deref().map(|note| redact.export_text(note)),
        reviewer: redact.export_text(&review.reviewer),
        created_at: review.created_at,
        has_recording: !context.recordings.is_empty(),
        recording_position,
    }
}

pub(crate) async fn export(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    let mut tx = state.app.pool().begin().await.map_err(db_error)?;
    sqlx::query("set transaction isolation level repeatable read read only")
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    let rows: Vec<ExportSource> = sqlx::query_as(
        "select c.*, r.case_spec->>'code' as case_code, r.case_version,
                r.case_spec->'data' as case_data, r.case_spec->'expected' as expected
         from current_reviews c join runs r on r.id = c.run_id
         order by c.run_id, c.item_idx nulls first",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_error)?;
    let mut output = Vec::new();
    if !rows.is_empty() {
        let redact = Redactor::load(&state.app, &mut tx).await?;
        for group in rows.chunk_by(|left, right| left.review.run_id == right.review.run_id) {
            let context = RunEvidence::load(&mut tx, group[0].review.run_id)
                .await
                .map_err(db_error)?;
            for row in group {
                serde_json::to_writer(&mut output, &export_row(row, &context, &redact))
                    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "无法序列化审核数据"))?;
                output.push(b'\n');
            }
        }
    }
    tx.commit().await.map_err(db_error)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=reviews.ndjson",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        output,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 审核导出行重新脱敏样本账号密码序列号口令和路径() {
        let username = "sample-account@example.test";
        let password = "fake-example-1A2B";
        let serial = "mock-device-A123";
        let token = "sample-review-token-012345";
        let redact = Redactor::new(
            [username, "sample-account", password, serial, token]
                .map(str::to_owned)
                .to_vec(),
        );
        let source = ExportSource {
            review: Review {
                id: Uuid::new_v4(),
                run_id: Uuid::new_v4(),
                item_idx: Some(1),
                mark: "correct".into(),
                judge_verdict: "uncertain".into(),
                note: Some(format!("补充\n{token} {serial}")),
                reviewer: format!("审核员 {username}"),
                created_at: Utc::now(),
            },
            case_code: format!("TC-04-{serial}"),
            case_version: 2,
            case_data: Some(json!({"target": password})),
            expected: vec![],
        };
        let context = RunEvidence {
            items: vec![EvidenceItem {
                idx: 1,
                expected_text: "出现 {{target}} 或 {{account.password}}".into(),
                evidence: format!(
                    "证据来自 sample-account；目录 /home/test/private/01_after.png；账号 {username}"
                ),
                file_refs: [
                    "shots/01_after.png",
                    "01_after.png",
                    "step:1",
                    "../private/hidden.png",
                    "x%2fy.png",
                ]
                .map(str::to_owned)
                .into(),
            }],
            steps: vec![],
            recordings: vec![],
        };
        let row = export_row(&source, &context, &redact);
        let encoded = serde_json::to_string(&row).unwrap();
        for forbidden in [
            username,
            "sample-account",
            password,
            serial,
            token,
            "account.password",
            "/home/",
            "shots/",
            "hidden.png",
            "device_serial",
        ] {
            assert!(!encoded.contains(forbidden), "导出行不应含敏感片段");
        }
        assert_eq!(row.expected_text, "出现 （已脱敏） 或 （已脱敏）");
        assert_eq!(row.file_refs, ["01_after.png"]);
        assert_eq!(row.human_verdict, "uncertain");
        assert_eq!(row.case_version, 2);
        assert!(!row.has_recording);
        assert!(row.recording_position.is_none());
        assert_eq!(encoded.lines().count(), 1);
    }
}
