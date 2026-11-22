//! 在 setup 和 teardown 之间按设备录屏；任何采集故障都不参与执行判定。

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::recording::{MAX_RECORDING_SIZE, RecordingIndex, RecordingSegment, recording_name};
use tokio::fs;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

const RECORD_TIMEOUT: Duration = Duration::from_secs(180);
const SEGMENT_SECS: &str = "170";
const BIT_RATE: &str = "2000000";
const GRACE_TIMEOUT: Duration = Duration::from_secs(8);
const FORCE_TIMEOUT: Duration = Duration::from_secs(2);
const PULL_TIMEOUT: Duration = Duration::from_secs(15);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const RETRIES: u32 = 3;

pub(crate) trait Adb: Send + Sync + 'static {
    type Process: AdbProcess;

    fn spawn(&self, serial: &str, args: &[&OsStr]) -> io::Result<Self::Process>;
}

pub(crate) trait AdbProcess: Send {
    fn wait(&mut self) -> impl Future<Output = io::Result<ExitStatus>> + Send;
}

pub(crate) struct SystemAdb;

impl Adb for SystemAdb {
    type Process = Child;

    fn spawn(&self, serial: &str, args: &[&OsStr]) -> io::Result<Child> {
        Command::new("adb")
            .args(["-s", serial])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
    }
}

impl AdbProcess for Child {
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        Child::wait(self).await
    }
}

type DeviceGuard = Arc<OwnedMutexGuard<()>>;
type Devices = Mutex<HashMap<String, Weak<AsyncMutex<()>>>>;

/// 取消后服务端可能立刻派新 run；同一台设备要等旧录屏收尾再进入下一次登录。
pub(crate) async fn lock_device(serial: &str) -> DeviceGuard {
    static DEVICES: LazyLock<Devices> = LazyLock::new(Devices::default);
    let device = {
        let mut devices = DEVICES.lock().unwrap();
        devices.retain(|_, device| device.strong_count() > 0);
        let slot = devices.entry(serial.to_owned()).or_default();
        slot.upgrade().unwrap_or_else(|| {
            let device = Arc::new(AsyncMutex::new(()));
            *slot = Arc::downgrade(&device);
            device
        })
    };
    Arc::new(device.lock_owned().await)
}

type Warn = Arc<dyn Fn(String) + Send + Sync>;

pub(crate) struct Recorder {
    stop: watch::Sender<bool>,
    stopped: Option<oneshot::Receiver<()>>,
    worker: JoinHandle<()>,
    warn: Warn,
}

impl Recorder {
    pub(crate) async fn start(
        serial: &str,
        run_dir: &Path,
        adb: impl Adb,
        device: DeviceGuard,
        warn: impl Fn(String) + Send + Sync + 'static,
        publish: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let (ready, started) = oneshot::channel();
        let (finished, stopped) = oneshot::channel();
        let warn: Warn = Arc::new(warn);
        let worker = Worker {
            adb,
            serial: serial.to_owned(),
            dir: run_dir.join("recording"),
            warn: warn.clone(),
        };
        let run_id = run_dir
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(|name| name.parse::<Uuid>().ok());
        let handle = Self {
            stop,
            stopped: Some(stopped),
            worker: tokio::spawn(async move {
                if let Some(run_id) = run_id {
                    worker.record(run_id, &mut stopping, ready).await;
                } else {
                    (worker.warn)("录屏目录缺少合法的 run 编号，已跳过录屏".into());
                }
                drop(device);
                let _ = finished.send(());
                wait_for_stop(&mut stopping).await;
                publish.await;
            }),
            warn,
        };
        let _ = started.await;
        handle
    }

    pub(crate) async fn stop(&mut self) {
        self.stop.send_replace(true);
        if let Some(stopped) = self.stopped.take() {
            let _ = stopped.await;
        }
    }

    pub(crate) async fn wait_for_upload(mut self) {
        self.stop().await;
        if (&mut self.worker).await.is_err() {
            (self.warn)("录屏后台任务异常退出，已跳过未完成的分段".into());
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

struct Worker<A> {
    adb: A,
    serial: String,
    dir: PathBuf,
    warn: Warn,
}

impl<A: Adb> Worker<A> {
    async fn record(
        self,
        run_id: Uuid,
        stop: &mut watch::Receiver<bool>,
        ready: oneshot::Sender<()>,
    ) {
        let mut index = RecordingIndex::default();
        if let Err(error) = fs::DirBuilder::new().mode(0o700).create(&self.dir).await {
            (self.warn)(format!(
                "创建录屏目录失败，已跳过录屏（{}）",
                io_detail(&error)
            ));
            return;
        }
        if let Err(error) = write_index(&self.dir, &index).await {
            (self.warn)(format!(
                "初始化录屏索引失败，已跳过录屏（{}）",
                io_detail(&error)
            ));
            return;
        }
        let mut ready = Some(ready);
        for seq in 1..=i32::MAX {
            if *stop.borrow() {
                break;
            }
            let remote = format!("/sdcard/hafa_rec_{run_id}_{seq}.mp4");
            let pattern = format!(
                "'^(/[^ ]*/)?screenrecord --time-limit {SEGMENT_SECS} --bit-rate {BIT_RATE} {}$'",
                remote.replace('.', "[.]")
            );
            let started = Instant::now();
            let start_epoch_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |delta| delta.as_millis() as i64);
            let args = [
                "shell",
                "exec",
                "screenrecord",
                "--time-limit",
                SEGMENT_SECS,
                "--bit-rate",
                BIT_RATE,
                &remote,
            ]
            .map(OsStr::new);
            let child = self.adb.spawn(&self.serial, &args);
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
            let mut child = match child {
                Ok(child) => child,
                Err(error) => {
                    (self.warn)(format!("启动录屏失败，已跳过录屏（{}）", io_detail(&error)));
                    break;
                }
            };
            let (stopped, duration, result) = tokio::select! {
                biased;
                _ = wait_for_stop(stop) => {
                    let duration = started.elapsed();
                    (true, duration, self.finish(&mut child, &pattern).await)
                }
                result = timeout(RECORD_TIMEOUT, child.wait()) => (false, started.elapsed(), check_status(result)),
            };
            drop(child);

            if let Err(reason) = result {
                (self.warn)(format!(
                    "录屏第 {seq} 段未正常结束，已跳过该段（{}）",
                    reason.detail()
                ));
                self.force_stop(&pattern).await;
                self.remove_remote(&remote).await;
                break;
            }

            let file = recording_name(seq);
            let temp = self.dir.join(format!(".{file}.part"));
            let pulled = self
                .retry(
                    "拉取录屏分段",
                    &[OsStr::new("pull"), OsStr::new(&remote), temp.as_os_str()],
                    PULL_TIMEOUT,
                )
                .await;
            let removed = self.remove_remote(&remote).await;
            if pulled {
                match save_segment(&temp, &self.dir.join(&file)).await {
                    Ok(()) => {
                        index.segments.push(RecordingSegment {
                            seq,
                            file,
                            start_epoch_ms,
                            duration_ms: (duration.as_millis() as i64).max(1),
                        });
                        if let Err(error) = write_index(&self.dir, &index).await {
                            (self.warn)(format!("保存录屏索引失败（{}）", io_detail(&error)));
                        }
                    }
                    Err(error) => {
                        (self.warn)(format!("保存录屏第 {seq} 段失败（{}）", io_detail(&error)))
                    }
                }
            }
            let _ = fs::remove_file(&temp).await;
            if stopped || !removed {
                break;
            }
        }
    }

    async fn finish(&self, child: &mut A::Process, pattern: &str) -> Result<(), CommandFailure> {
        let interrupt = async {
            let args = ["shell", "pkill", "-2", "-f", pattern].map(OsStr::new);
            for attempt in 0..=RETRIES {
                match self.command(&args, CONTROL_TIMEOUT).await {
                    Ok(()) | Err(CommandFailure::Timeout) => break,
                    Err(_) if attempt < RETRIES => retry_delay(attempt).await,
                    Err(_) => break,
                }
            }
        };
        check_status(
            timeout(GRACE_TIMEOUT, async {
                tokio::select! {
                    biased;
                    result = child.wait() => result,
                    _ = interrupt => child.wait().await,
                }
            })
            .await,
        )
    }

    async fn force_stop(&self, pattern: &str) {
        let command = format!(
            "pkill -9 -f {pattern}; for attempt in 1 2 3 4; do \
            pgrep -f {pattern} >/dev/null; case \"$?\" in 1) exit 0;; 0) sleep 0.1;; *) exit 2;; esac; done; exit 1"
        );
        if let Err(reason) = self
            .command(&[OsStr::new("shell"), OsStr::new(&command)], FORCE_TIMEOUT)
            .await
        {
            (self.warn)(format!(
                "无法确认设备上的录屏进程已退出（{}）",
                reason.detail()
            ));
        }
    }

    async fn remove_remote(&self, remote: &str) -> bool {
        self.retry(
            "清理设备录屏临时文件",
            &["shell", "rm", "-f", remote].map(OsStr::new),
            CONTROL_TIMEOUT,
        )
        .await
    }

    async fn command(&self, args: &[&OsStr], limit: Duration) -> Result<(), CommandFailure> {
        let mut child = self
            .adb
            .spawn(&self.serial, args)
            .map_err(CommandFailure::Io)?;
        check_status(timeout(limit, child.wait()).await)
    }

    async fn retry(&self, operation: &str, args: &[&OsStr], limit: Duration) -> bool {
        for attempt in 0..=RETRIES {
            match self.command(args, limit).await {
                Ok(()) => return true,
                Err(_) if attempt < RETRIES => retry_delay(attempt).await,
                Err(reason) => (self.warn)(format!(
                    "{operation}失败，已重试 {RETRIES} 次并跳过（{}）",
                    reason.detail()
                )),
            }
        }
        false
    }
}

enum CommandFailure {
    Io(io::Error),
    Exit(ExitStatus),
    Timeout,
}

impl CommandFailure {
    fn detail(&self) -> String {
        match self {
            Self::Io(error) => io_detail(error),
            Self::Exit(status) => format!("adb 进程状态：{status}"),
            Self::Timeout => "adb 等待超时".into(),
        }
    }
}

fn io_detail(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("系统错误码 {code}"),
        None => format!("错误类型 {:?}", error.kind()),
    }
}

type WaitResult = Result<io::Result<ExitStatus>, tokio::time::error::Elapsed>;

fn check_status(wait: WaitResult) -> Result<(), CommandFailure> {
    let status = wait
        .map_err(|_| CommandFailure::Timeout)?
        .map_err(CommandFailure::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(CommandFailure::Exit(status))
    }
}

async fn retry_delay(attempt: u32) {
    sleep(Duration::from_millis(150 * u64::from(attempt + 1))).await;
}

async fn save_segment(temp: &Path, path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(temp).await?;
    if !meta.is_file() || !(8..=MAX_RECORDING_SIZE).contains(&meta.len()) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    fs::rename(temp, path).await
}

async fn write_index(dir: &Path, index: &RecordingIndex) -> io::Result<()> {
    let bytes = serde_json::to_vec(index)?;
    let temp = dir.join(format!(".index-{}.tmp", Uuid::new_v4()));
    let result = async {
        fs::write(&temp, bytes).await?;
        fs::rename(&temp, dir.join("index.json")).await
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    result
}

#[cfg(test)]
#[path = "recorder_tests.rs"]
pub(crate) mod tests;
