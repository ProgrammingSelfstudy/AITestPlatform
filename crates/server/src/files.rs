//! 节点上传和浏览器读取运行证据。

use std::collections::HashSet;
use std::io;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Path as RoutePath, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use protocol::recording::{MAX_RECORDING_INDEX_SIZE, MAX_RECORDING_SIZE, RecordingIndex, recording_seq};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{api::ApiState, node_identity};

pub(crate) const MAX_SIZE: usize = 10 * 1024 * 1024;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Copy)]
enum EvidenceKind {
    Shot,
    Page,
    Recording(i32),
}

impl EvidenceKind {
    fn from_name(name: &str) -> Option<Self> {
        if name.len() > 255
            || name.contains("..")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return None;
        }
        match name.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Shot),
            "txt" => Some(Self::Page),
            "mp4" => recording_seq(name).map(Self::Recording),
            _ => None,
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Shot => "image/png",
            Self::Page => "text/plain; charset=utf-8",
            Self::Recording(_) => "video/mp4",
        }
    }

    fn database_kind(self) -> &'static str {
        match self {
            Self::Shot => "shot",
            Self::Page => "page",
            Self::Recording(_) => "recording",
        }
    }

    fn valid_body(self, body: &[u8]) -> bool {
        match self {
            Self::Shot => body.starts_with(PNG_MAGIC),
            Self::Page => std::str::from_utf8(body).is_ok(),
            Self::Recording(_) => body.get(4..8) == Some(b"ftyp"),
        }
    }

    fn seq(self) -> i32 {
        match self {
            Self::Recording(seq) => seq,
            Self::Shot | Self::Page => 0,
        }
    }
}

pub(crate) fn valid_reference(reference: &str) -> bool {
    let name = reference.strip_prefix("shots/").unwrap_or(reference);
    matches!(EvidenceKind::from_name(name), Some(EvidenceKind::Shot | EvidenceKind::Page))
}

pub(crate) fn valid_judge_reference(reference: &str) -> bool {
    if let Some(step) = reference.strip_prefix("step:") {
        return (1..=4).contains(&step.len()) && step.bytes().all(|b| b.is_ascii_digit());
    }
    valid_reference(reference)
}

pub(crate) fn prefix(run_id: Uuid) -> String {
    format!("files/{run_id}/")
}

pub(crate) async fn names(
    conn: &mut sqlx::PgConnection,
    run_id: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    let prefix = prefix(run_id);
    let paths: Vec<String> =
        sqlx::query_scalar("select distinct path from files where run_id = $1 and kind <> 'recording' order by path")
            .bind(run_id)
            .fetch_all(conn)
            .await?;
    Ok(paths
        .iter()
        .filter_map(|p| p.strip_prefix(&prefix))
        .filter(|name| matches!(EvidenceKind::from_name(name), Some(EvidenceKind::Shot | EvidenceKind::Page)))
        .map(str::to_owned)
        .collect())
}

#[derive(Serialize)]
pub(crate) struct RecordingView {
    pub(crate) seq: i32,
    url: String,
    size: i64,
    pub(crate) start_epoch_ms: Option<i64>,
    pub(crate) duration_ms: Option<i64>,
}

pub(crate) async fn recordings(conn: &mut sqlx::PgConnection, run_id: Uuid) -> Result<Vec<RecordingView>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i32, String, i64, Option<i64>, Option<i64>)>(
        "select seq, path, size, start_epoch_ms, duration_ms from files
         where run_id = $1 and kind = 'recording' order by seq",
    )
    .bind(run_id)
    .fetch_all(conn)
    .await?;
    let prefix = prefix(run_id);
    Ok(rows.into_iter().filter_map(|(seq, path, size, start_epoch_ms, duration_ms)| {
        let name = path.strip_prefix(&prefix)?;
        (recording_seq(name) == Some(seq)).then(|| RecordingView {
            seq, url: format!("/api/runs/{run_id}/files/{name}"), size, start_epoch_ms, duration_ms,
        })
    }).collect())
}

fn hidden_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "证据目录或文件不在指定位置",
    )
}

async fn checked_dir(path: PathBuf, create: bool) -> io::Result<PathBuf> {
    if create {
        match fs::create_dir(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    if fs::canonicalize(&path).await? != path || !fs::symlink_metadata(&path).await?.is_dir() {
        return Err(hidden_path());
    }
    Ok(path)
}

pub(crate) async fn run_dir(base: &Path, run_id: Uuid, create: bool) -> io::Result<PathBuf> {
    if create {
        fs::create_dir_all(base).await?;
    }
    let base = fs::canonicalize(base).await?;
    let files = checked_dir(base.join("files"), create).await?;
    checked_dir(files.join(run_id.to_string()), create).await
}

async fn open_registered_file(base: &Path, run_id: Uuid, name: &str) -> io::Result<File> {
    let dir = run_dir(base, run_id, false).await?;
    let path = dir.join(name);
    if path.parent() != Some(&dir) || fs::canonicalize(&path).await? != path || !fs::symlink_metadata(&path).await?.is_file() {
        return Err(hidden_path());
    }
    File::open(path).await
}

async fn read_small_file(file: File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take((MAX_SIZE + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_SIZE {
        return Err(hidden_path());
    }
    Ok(bytes)
}

fn byte_range(headers: &HeaderMap, size: u64) -> Result<Option<RangeInclusive<u64>>, ()> {
    let mut values = headers.get_all(header::RANGE).iter();
    let Some(value) = values.next() else { return Ok(None) };
    if values.next().is_some() {
        return Err(());
    }
    let last = size.checked_sub(1).ok_or(())?;
    let (unit, interval) = value.to_str().map_err(|_| ())?.trim().split_once('=').ok_or(())?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return Err(());
    }
    let (first, end) = interval.split_once('-').ok_or(())?;
    let decimal = |text: &str| {
        if text.is_empty() || !text.bytes().all(|digit| digit.is_ascii_digit()) {
            return Err(());
        }
        Ok(text.bytes().fold(0_u64, |number, digit| {
            number.saturating_mul(10).saturating_add(u64::from(digit - b'0'))
        }))
    };
    let (first, end) = if first.is_empty() {
        let suffix = decimal(end)?;
        if suffix == 0 {
            return Err(());
        }
        (size.saturating_sub(suffix), last)
    } else {
        (decimal(first)?, if end.is_empty() { last } else { decimal(end)?.min(last) })
    };
    if first > end {
        return Err(());
    }
    Ok(Some(first..=end))
}

async fn recording_response(mut file: File, headers: &HeaderMap) -> Result<Response, StatusCode> {
    let size = file.metadata().await.map_err(|_| StatusCode::NOT_FOUND)?.len();
    if !(8..=MAX_RECORDING_SIZE).contains(&size) {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut response = match byte_range(headers, size) {
        Err(()) => (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{size}"))]).into_response(),
        Ok(range) => {
            let (status, count, content_range) = if let Some(range) = range {
                let (first, end) = range.into_inner();
                file.seek(SeekFrom::Start(first)).await.map_err(|_| StatusCode::NOT_FOUND)?;
                (StatusCode::PARTIAL_CONTENT, end - first + 1, Some(format!("bytes {first}-{end}/{size}")))
            } else {
                (StatusCode::OK, size, None)
            };
            let chunks = ReaderStream::with_capacity(file.take(count), 64 * 1024);
            let mut response = (status, [(header::CONTENT_LENGTH, count.to_string())], Body::from_stream(chunks)).into_response();
            if let Some(content_range) = content_range {
                response.headers_mut().insert(header::CONTENT_RANGE, content_range.parse().expect("字节区间为十进制数"));
            }
            response
        }
    };
    response.headers_mut().insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    Ok(response)
}

pub(crate) async fn get(
    State(state): State<ApiState>,
    RoutePath((id, name)): RoutePath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let run_id = id.parse().map_err(|_| StatusCode::NOT_FOUND)?;
    let kind = EvidenceKind::from_name(&name).ok_or(StatusCode::NOT_FOUND)?;
    let path = prefix(run_id) + &name;
    let registered: bool =
        sqlx::query_scalar("select exists(select 1 from files where run_id = $1 and path = $2
                            and ($3 = 0 or (kind = 'recording' and seq = $3)))")
            .bind(run_id)
            .bind(path)
            .bind(kind.seq())
            .fetch_one(state.app.pool())
            .await
            .map_err(database_error)?;
    if !registered {
        return Err(StatusCode::NOT_FOUND);
    }
    let file = open_registered_file(&state.paths.data, run_id, &name)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let response = if matches!(kind, EvidenceKind::Recording(_)) {
        recording_response(file, &headers).await?
    } else if matches!(kind, EvidenceKind::Page) {
        // 终态页面文字可能带出界面上显示的账号信息：读取时按已登记的凭证脱敏，脱敏不可用就拒绝返回
        let bytes = read_small_file(file).await.map_err(|_| StatusCode::NOT_FOUND)?;
        let text = String::from_utf8_lossy(&bytes);
        let mut conn = state.app.pool().acquire().await.map_err(database_error)?;
        let redactor = crate::reviews::Redactor::load(&state.app, &mut conn).await.map_err(|(status, _)| status)?;
        redactor.text(&text).into_response()
    } else {
        read_small_file(file).await.map_err(|_| StatusCode::NOT_FOUND)?.into_response()
    };
    Ok((
        [
            (header::CONTENT_TYPE, kind.content_type()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        response,
    )
        .into_response())
}

fn database_error(error: sqlx::Error) -> StatusCode {
    tracing::error!("查询或登记证据文件失败：{error}");
    StatusCode::INTERNAL_SERVER_ERROR
}

fn disk_error(error: io::Error) -> StatusCode {
    tracing::error!("写入证据文件失败：{error}");
    StatusCode::INTERNAL_SERVER_ERROR
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn write_identity(state: &ApiState, id: &str, headers: &HeaderMap) -> Result<(Uuid, Uuid), StatusCode> {
    let node_id = node_identity(&state.app, headers).await?;
    let run_id: Uuid = id.parse().map_err(|_| StatusCode::NOT_FOUND)?;
    let pool = state.app.pool();
    let owner: Option<(Uuid, bool)> = sqlx::query_as(
        "select node_id, status not in ('finished', 'blocked', 'cancelled')
                or coalesce(finished_at >= clock_timestamp() - interval '10 minutes', false)
         from runs where id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    .map_err(database_error)?;
    let (owner, allowed) = owner.ok_or(StatusCode::NOT_FOUND)?;
    if owner != node_id || !allowed {
        return Err(StatusCode::FORBIDDEN);
    }

    Ok((run_id, node_id))
}

async fn lock_write(conn: &mut sqlx::PgConnection, run_id: Uuid, node_id: Uuid) -> Result<(), StatusCode> {
    let allowed: Option<bool> = sqlx::query_scalar(
        "select status not in ('finished', 'blocked', 'cancelled')
                or coalesce(finished_at >= clock_timestamp() - interval '10 minutes', false)
         from runs where id = $1 and node_id = $2 for update",
    )
    .bind(run_id)
    .bind(node_id)
    .fetch_optional(conn)
    .await
    .map_err(database_error)?;
    if allowed != Some(true) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

enum Upload {
    Small(Bytes),
    Recording(Body),
}

async fn write_recording(body: Body, kind: EvidenceKind, file: &mut File) -> Result<(i64, String), StatusCode> {
    let mut chunks = body.into_data_stream();
    let mut size = 0_u64;
    let mut magic = Vec::with_capacity(8);
    let mut hash = Sha256::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        size = size.saturating_add(chunk.len() as u64);
        if size > MAX_RECORDING_SIZE {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        if magic.len() < 8 {
            magic.extend_from_slice(&chunk[..(8 - magic.len()).min(chunk.len())]);
            if magic.len() == 8 && !kind.valid_body(&magic) {
                return Err(StatusCode::BAD_REQUEST);
            }
        }
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(disk_error)?;
    }
    if !kind.valid_body(&magic) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok((size as i64, hex::encode(hash.finalize())))
}

pub(crate) async fn put(
    State(state): State<ApiState>,
    RoutePath((id, name)): RoutePath<(String, String)>,
    request: Request,
) -> Result<StatusCode, StatusCode> {
    let (run_id, node_id) = write_identity(&state, &id, request.headers()).await?;
    let kind = EvidenceKind::from_name(&name).ok_or(StatusCode::BAD_REQUEST)?;
    let upload = if matches!(kind, EvidenceKind::Recording(_)) {
        let too_large = request.headers().get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|size| size > MAX_RECORDING_SIZE);
        if too_large {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        Upload::Recording(request.into_body())
    } else {
        let bytes = to_bytes(request.into_body(), MAX_SIZE).await.map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
        if !kind.valid_body(&bytes) {
            return Err(StatusCode::BAD_REQUEST);
        }
        Upload::Small(bytes)
    };
    let dir = run_dir(&state.paths.data, run_id, true)
        .await
        .map_err(disk_error)?;
    let temp_path = dir.join(format!(".upload-{}.tmp", Uuid::new_v4()));
    let mut handle = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .await
        .map_err(disk_error)?;
    let temp = TempFile(temp_path);
    let (size, hash) = match upload {
        Upload::Small(bytes) => {
            handle.write_all(&bytes).await.map_err(disk_error)?;
            (bytes.len() as i64, hex::encode(Sha256::digest(&bytes)))
        }
        Upload::Recording(body) => write_recording(body, kind, &mut handle).await?,
    };
    handle.sync_all().await.map_err(disk_error)?;
    drop(handle);

    // 锁住 run 后复核补传窗口。同一个 run 的登记和覆盖按顺序生效，磁盘失败时回滚登记。
    let mut tx = state.app.pool().begin().await.map_err(database_error)?;
    lock_write(&mut tx, run_id, node_id).await?;
    let change = sqlx::query(
        "insert into files (run_id, kind, seq, path, size, sha256) values ($1,$2,$3,$4,$5,$6)
         on conflict (run_id, lower(path)) where kind in ('shot', 'page', 'recording')
         do update set seq = excluded.seq, size = excluded.size, sha256 = excluded.sha256,
            start_epoch_ms = case when files.sha256 = excluded.sha256 then files.start_epoch_ms end,
            duration_ms = case when files.sha256 = excluded.sha256 then files.duration_ms end
         where files.path = excluded.path and files.kind = excluded.kind",
    )
    .bind(run_id).bind(kind.database_kind()).bind(kind.seq()).bind(prefix(run_id) + &name).bind(size).bind(hash)
    .execute(&mut *tx).await.map_err(database_error)?;
    if change.rows_affected() == 0 {
        return Err(StatusCode::CONFLICT);
    }

    fs::rename(&temp.0, dir.join(name))
        .await
        .map_err(disk_error)?;
    tx.commit().await.map_err(database_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn put_recording_index(
    State(state): State<ApiState>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Result<StatusCode, StatusCode> {
    let (run_id, node_id) = write_identity(&state, &id, request.headers()).await?;
    let bytes = to_bytes(request.into_body(), MAX_RECORDING_INDEX_SIZE).await.map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    let index: RecordingIndex = serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    let mut unique = HashSet::new();
    if !index.segments.iter().all(|segment| segment.is_valid() && unique.insert(segment.seq)) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut tx = state.app.pool().begin().await.map_err(database_error)?;
    lock_write(&mut tx, run_id, node_id).await?;
    let prefix = prefix(run_id);
    let seqs: Vec<_> = index.segments.iter().map(|s| s.seq).collect();
    let paths: Vec<_> = index.segments.iter().map(|s| format!("{prefix}{}", s.file)).collect();
    let starts: Vec<_> = index.segments.iter().map(|s| s.start_epoch_ms).collect();
    let durations: Vec<_> = index.segments.iter().map(|s| s.duration_ms).collect();
    let changed = sqlx::query(
        "update files as f set start_epoch_ms = s.start_epoch_ms, duration_ms = s.duration_ms
         from unnest($2::int[], $3::text[], $4::bigint[], $5::bigint[])
              as s(seq, path, start_epoch_ms, duration_ms)
         where f.run_id = $1 and f.kind = 'recording' and f.seq = s.seq and f.path = s.path",
    )
    .bind(run_id).bind(&seqs).bind(&paths).bind(&starts).bind(&durations)
    .execute(&mut *tx).await.map_err(database_error)?;
    if changed.rows_affected() != index.segments.len() as u64 {
        return Err(StatusCode::CONFLICT);
    }
    tx.commit().await.map_err(database_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn 判定步骤文字引用只允许一到四位数字() {
        let mut references = vec![
            "01_after.png",
            "shots/02_after.png",
            "step:1",
            "step:0008",
            "step:9999",
            "step:",
            "step:abc",
            "step:../x",
            "step:12345",
            "step:８",
            "step:-1",
            "shots/step:8",
        ];
        references.retain(|reference| valid_judge_reference(reference));
        assert_eq!(
            references,
            [
                "01_after.png",
                "shots/02_after.png",
                "step:1",
                "step:0008",
                "step:9999"
            ]
        );
        assert!(!valid_reference("step:8"));
    }

    #[tokio::test]
    async fn 读取证据只允许规范化后的路径留在该run目录() {
        let run_id = Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("platform-evidence-{run_id}"));
        let dir = run_dir(&root, run_id, true).await.unwrap();
        let local = dir.join("inside.png");
        let outside = root.join("outside.png");
        fs::write(&local, PNG_MAGIC).await.unwrap();
        fs::write(&outside, PNG_MAGIC).await.unwrap();
        let file = open_registered_file(&root, run_id, "inside.png").await.unwrap();
        assert_eq!(read_small_file(file).await.unwrap(), PNG_MAGIC);
        symlink(&outside, dir.join("escaped.png")).unwrap();
        assert!(
            open_registered_file(&root, run_id, "escaped.png")
                .await
                .is_err()
        );
        let elsewhere = root.join("elsewhere");
        fs::rename(&dir, &elsewhere).await.unwrap();
        symlink(&elsewhere, &dir).unwrap();
        assert!(
            open_registered_file(&root, run_id, "inside.png")
                .await
                .is_err()
        );
        fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn 录屏魔数跨请求分块仍可校验() {
        let temp = TempFile(std::env::temp_dir().join(format!("recording-magic-{}", Uuid::new_v4())));
        let mut file = File::create(&temp.0).await.unwrap();
        let sample = b"\0\0\0\x0cftypisom";
        let kind = EvidenceKind::from_name("rec_01.mp4").unwrap();
        let chunks = futures_util::stream::iter(sample.chunks(3).map(|chunk| Ok::<_, io::Error>(Bytes::copy_from_slice(chunk))));
        let (size, hash) = write_recording(Body::from_stream(chunks), kind, &mut file).await.unwrap();
        file.flush().await.unwrap();
        assert_eq!(size as usize, sample.len());
        assert_eq!(hash, hex::encode(Sha256::digest(sample)));
        assert_eq!(fs::read(&temp.0).await.unwrap(), sample);
        for invalid in [&b"ftyp\0\0\0\x0c"[..], &b"\0\0\0\x0cfty"[..]] {
            assert_eq!(write_recording(Body::from(invalid), kind, &mut file).await, Err(StatusCode::BAD_REQUEST));
        }
    }

    #[tokio::test]
    async fn 录屏在大文件尾部只流式读取请求区间() {
        let temp = TempFile(std::env::temp_dir().join(format!("recording-range-{}", Uuid::new_v4())));
        let mut file = File::create(&temp.0).await.unwrap();
        file.seek(SeekFrom::Start(MAX_RECORDING_SIZE - 4)).await.unwrap();
        file.write_all(b"tail").await.unwrap();
        file.flush().await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=-4".parse().unwrap());
        let response = recording_response(File::open(&temp.0).await.unwrap(), &headers).await.unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], format!("bytes {}-{}/{MAX_RECORDING_SIZE}", MAX_RECORDING_SIZE - 4, MAX_RECORDING_SIZE - 1));
        assert_eq!(to_bytes(response.into_body(), 4).await.unwrap(), "tail");
    }

    #[tokio::test]
    async fn 录屏无区间头返回200且拒绝多个区间() {
        let temp = TempFile(std::env::temp_dir().join(format!("recording-full-{}", Uuid::new_v4())));
        let sample = b"\0\0\0\x0cftypisom";
        fs::write(&temp.0, sample).await.unwrap();
        let mut headers = HeaderMap::new();
        let response = recording_response(File::open(&temp.0).await.unwrap(), &headers).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "12");
        assert_eq!(to_bytes(response.into_body(), 12).await.unwrap().as_ref(), sample);
        headers.append(header::RANGE, "bytes=0-3".parse().unwrap());
        headers.append(header::RANGE, "bytes=4-7".parse().unwrap());
        let response = recording_response(File::open(&temp.0).await.unwrap(), &headers).await.unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */12");
    }
}
