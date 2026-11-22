//! 执行期间上报轨迹，独立上传截图、终态页面和录屏。

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent::codex_exec::events::Summary;
use futures_util::{StreamExt, TryStreamExt, stream};
use http_body::Frame;
use http_body_util::StreamBody;
use protocol::recording::{MAX_RECORDING_INDEX_SIZE, MAX_RECORDING_SIZE, RecordingIndex};
use reqwest::{Client, RequestBuilder, Url, header};
use serde::Deserialize;
use serde_json::json;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::io::ReaderStream;

use crate::{Reporter, platform_client};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RECORDING_TIMEOUT: Duration = Duration::from_secs(45);
const RETRIES: usize = 3;
const PARALLEL_UPLOADS: usize = 8;
const MAX_SIZE: u64 = 10 * 1024 * 1024;

#[derive(Deserialize)]
pub(crate) struct TraceStep {
    idx: u32,
    #[serde(default)]
    at_ms: i64,
    action: String,
    target: Option<String>,
    point: Option<(i32, i32)>,
    value: Option<String>,
    shot: Option<String>,
    #[serde(default)]
    new_labels: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct Trace {
    pub steps: Vec<TraceStep>,
    pub result: Option<String>,
    pub reason: Option<String>,
}

pub(crate) async fn read_trace(dir: &Path) -> Option<Trace> {
    serde_json::from_slice(&fs::read(dir.join("trace.json")).await.ok()?).ok()
}

pub(crate) struct Execution {
    pub summary: Summary,
    pub trace: Option<Trace>,
    pub uploaded: HashSet<String>,
}

type Job = (String, PathBuf);

#[derive(Default)]
struct Progress {
    steps: HashSet<u32>,
    queued: HashSet<String>,
}

impl Progress {
    async fn report(
        &mut self,
        dir: &Path,
        rep: &Reporter,
        trace: Option<&Trace>,
        final_pass: bool,
        jobs: &UnboundedSender<Job>,
    ) {
        let mut completed_shots = HashSet::new();
        for step in trace
            .into_iter()
            .flat_map(|trace| &trace.steps)
            .filter(|step| step.idx > 0)
        {
            let reference = step
                .shot
                .as_deref()
                .and_then(|path| path.strip_prefix("shots/"))
                .filter(|name| shot_name(name));
            let number = reference
                .and_then(|name| name.split_once('_'))
                .map_or_else(|| format!("{:02}", step.idx), |(n, _)| n.to_owned());
            completed_shots.insert(number);
            if self.steps.insert(step.idx) || final_pass {
                rep.step(
                    step.idx,
                    step.at_ms,
                    &step.action,
                    json!({"target": step.target, "point": step.point, "value": step.value}),
                    json!({"new_labels": step.new_labels}),
                    reference.map(|name| format!("shots/{name}")),
                );
            }
        }
        // 观察图在动作前落盘，原图和轨迹在动作后落盘。只在该步完成后发布这一组图片。
        let shots = dir.join("shots");
        if fs::symlink_metadata(&shots).await.is_ok_and(|m| m.is_dir()) {
            if let Ok(mut entries) = fs::read_dir(shots).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let Some(name) = entry
                        .file_name()
                        .into_string()
                        .ok()
                        .filter(|name| shot_name(name))
                    else {
                        continue;
                    };
                    let step_finished = name
                        .split_once('_')
                        .is_some_and(|(number, _)| completed_shots.contains(number));
                    if (final_pass || step_finished)
                        && entry.file_type().await.is_ok_and(|ty| ty.is_file())
                    {
                        self.enqueue(name, entry.path(), jobs);
                    }
                }
            }
        }
        let page = dir.join("final_page.txt");
        if final_pass && fs::symlink_metadata(&page).await.is_ok_and(|m| m.is_file()) {
            self.enqueue("final_page.txt".into(), page, jobs);
        }
    }

    fn enqueue(&mut self, name: String, path: PathBuf, jobs: &UnboundedSender<Job>) {
        if self.queued.insert(name.clone()) {
            let _ = jobs.send((name, path));
        }
    }
}

fn shot_name(name: &str) -> bool {
    name.ends_with(".png")
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

async fn upload_request(
    client: Option<&Client>,
    base: &Url,
    token: &str,
    rep: &Reporter,
    name: &str,
    path: &Path,
) -> Result<RequestBuilder, &'static str> {
    let client = client.ok_or("无法初始化证据上传请求")?;
    let url = base
        .join(&format!("/api/runs/{}/files/{name}", rep.run_id()))
        .map_err(|_| "证据上传地址无效")?;
    let mut bytes = Vec::new();
    // 文件系统只读取白名单内的产物，并限制读取大小。
    let (file, _) = open_evidence(path).await?;
    file.take(MAX_SIZE + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "无法读取证据文件")?;
    if bytes.len() as u64 > MAX_SIZE {
        return Err("证据文件超过 10 MB");
    }
    let content_type = if name.ends_with(".png") {
        "image/png"
    } else {
        "text/plain; charset=utf-8"
    };
    Ok(crate::with_node_secret(client
        .put(url)
        .bearer_auth(token)
        .header("X-Node-Id", rep.node_id().to_string()))
        .header("Content-Type", content_type)
        .body(bytes))
}

async fn open_evidence(path: &Path) -> Result<(fs::File, u64), &'static str> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .await
        .map_err(|_| "无法打开证据文件")?;
    let metadata = file.metadata().await.map_err(|_| "无法读取证据文件属性")?;
    if !metadata.is_file() {
        return Err("证据必须是普通文件");
    }
    Ok((file, metadata.len()))
}

async fn upload<R, B>(
    name: &str,
    prepare: impl Fn() -> B,
    send: &impl Fn(RequestBuilder) -> R,
    warn: impl Fn(String),
) -> bool
where
    R: Future<Output = reqwest::Result<reqwest::Response>>,
    B: Future<Output = Result<RequestBuilder, &'static str>>,
{
    for attempt in 0..=RETRIES {
        let request = match prepare().await {
            Ok(request) => request,
            Err(reason) => {
                warn(format!("跳过证据 {name}（{reason}）"));
                break;
            }
        };
        let failure = match send(request).await {
            Ok(response) if response.status().is_success() => return true,
            Ok(response) => format!("HTTP {}", response.status().as_u16()),
            Err(_) => "连接失败或超时".into(),
        };
        if attempt == RETRIES {
            warn(format!("证据 {name} 上传失败，已重试 {RETRIES} 次并跳过（{failure}）"));
        } else {
            warn(format!("证据 {name} 上传失败，准备第 {} 次重试（{failure}）", attempt + 1));
            tokio::time::sleep(Duration::from_millis(150 * (attempt as u64 + 1))).await;
        }
    }
    false
}

pub(crate) async fn track_execution<F, R>(
    dir: &Path,
    rep: &Reporter,
    base: &Url,
    token: &str,
    execute: F,
    send: impl Fn(RequestBuilder) -> R + Sync,
) -> Execution
where
    F: Future<Output = Summary>,
    R: Future<Output = reqwest::Result<reqwest::Response>>,
{
    let client = platform_client(REQUEST_TIMEOUT).ok();
    let (jobs, pending) = mpsc::unbounded_channel();
    let track = async move {
        tokio::pin!(execute);
        let mut progress = Progress::default();
        let mut ticks = tokio::time::interval(POLL_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let summary = loop {
            tokio::select! {
                summary = &mut execute => break summary,
                _ = ticks.tick() => {
                    let trace = read_trace(dir).await;
                    progress.report(dir, rep, trace.as_ref(), false, &jobs).await;
                }
            }
        };
        let trace = read_trace(dir).await;
        // 收尾重发步骤，服务端按（run, idx）去重。完成队列交接后才发布判定，避免出现尚未上传的引用。
        progress.report(dir, rep, trace.as_ref(), true, &jobs).await;
        (summary, trace)
    };
    let uploads = stream::unfold(pending, |mut pending| async {
        pending.recv().await.map(|job| (job, pending))
    })
    .map(|(name, path)| {
        let client = client.as_ref();
        let send = &send;
        async move {
            match upload_request(client, base, token, rep, &name, &path).await {
                Ok(request) => upload(
                    &name,
                    || std::future::ready(request.try_clone().ok_or("证据请求无法重传")),
                    send,
                    |msg| rep.log("warn", msg),
                ).await.then_some(name),
                Err(reason) => {
                    rep.log("warn", format!("跳过证据 {name}（{reason}）"));
                    None
                }
            }
        }
    })
    .buffer_unordered(PARALLEL_UPLOADS)
    .filter_map(std::future::ready)
    .collect::<HashSet<_>>();
    let ((summary, trace), uploaded) = tokio::join!(track, uploads);
    Execution {
        summary,
        trace,
        uploaded,
    }
}

pub(crate) async fn upload_recordings<R>(
    dir: &Path, rep: &Reporter, base: &Url, token: &str, send: &impl Fn(RequestBuilder) -> R,
)
where
    R: Future<Output = reqwest::Result<reqwest::Response>>,
{
    let dir = dir.join("recording");
    if !fs::symlink_metadata(&dir).await.is_ok_and(|m| m.is_dir()) {
        return;
    }
    let Some(index) = read_recording_index(&dir).await else {
        rep.recording_warn("无法读取录屏索引，已跳过录屏上传".into());
        return;
    };
    if index.segments.is_empty() {
        return;
    }
    let Ok(client) = platform_client(RECORDING_TIMEOUT) else {
        rep.recording_warn("无法初始化录屏上传请求".into());
        return;
    };
    let put = |url| crate::with_node_secret(client.put(url).bearer_auth(token).header("X-Node-Id", rep.node_id().to_string()));
    let mut published = RecordingIndex::default();
    for segment in index.segments {
        if !segment.is_valid() {
            rep.recording_warn("录屏索引包含无效分段，已跳过该段".into());
            continue;
        }
        let Ok(url) = base.join(&format!("/api/runs/{}/files/{}", rep.run_id(), segment.file)) else {
            rep.recording_warn("录屏上传地址无效".into());
            break;
        };
        let path = dir.join(&segment.file);
        if upload(&segment.file, || async {
            let (file, size) = open_evidence(&path).await?;
            if !(8..=MAX_RECORDING_SIZE).contains(&size) {
                return Err("录屏文件大小不在允许范围内（上限 150 MB）");
            }
            let chunks = ReaderStream::with_capacity(file.take(size), 64 * 1024).map_ok(Frame::data);
            Ok(put(url.clone())
                .header(header::CONTENT_TYPE, "video/mp4")
                .header(header::CONTENT_LENGTH, size)
                .body(reqwest::Body::wrap(StreamBody::new(chunks))))
        }, send, |msg| rep.recording_warn(msg)).await {
            published.segments.push(segment);
        }
    }

    if !published.segments.is_empty() {
        let Ok(url) = base.join(&format!("/api/runs/{}/recording-index", rep.run_id())) else {
            rep.recording_warn("录屏索引上传地址无效".into());
            return;
        };
        upload("录屏索引", || std::future::ready(Ok(put(url.clone()).json(&published))), send, |msg| rep.recording_warn(msg)).await;
    }
}

async fn read_recording_index(dir: &Path) -> Option<RecordingIndex> {
    let (file, size) = open_evidence(&dir.join("index.json")).await.ok()?;
    if size > MAX_RECORDING_INDEX_SIZE as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORDING_INDEX_SIZE as u64 + 1).read_to_end(&mut bytes).await.ok()?;
    if bytes.len() > MAX_RECORDING_INDEX_SIZE {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
