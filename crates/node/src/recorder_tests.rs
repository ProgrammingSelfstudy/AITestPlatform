use super::*;
use serde_json::json;
use std::collections::HashSet;
use std::os::unix::process::ExitStatusExt;

pub(crate) const FAKE_MP4: &[u8] = b"\0\0\0\x10ftypisom\0\0\0\0";

#[derive(Default)]
struct FakeState {
    calls: Vec<(String, Vec<String>)>,
    active: HashMap<String, (String, oneshot::Sender<i32>)>,
    files: HashSet<(String, String)>,
}

#[derive(Clone, Default)]
pub(crate) struct FakeAdb {
    state: Arc<Mutex<FakeState>>,
    pub(crate) phases: Option<PathBuf>,
    pub(crate) start_error: Option<&'static str>,
    pub(crate) pull_error: Option<&'static str>,
    ignore_interrupt: bool,
}

pub(crate) enum FakeProcess {
    Immediate(i32),
    Recording(oneshot::Receiver<i32>),
}

impl AdbProcess for FakeProcess {
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        let code = match self {
            Self::Immediate(code) => *code,
            Self::Recording(done) => done.await.unwrap_or(1),
        };
        Ok(ExitStatus::from_raw(code << 8))
    }
}

impl FakeAdb {
    fn write_phase(&self, phase: &str) {
        if let Some(path) = &self.phases {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
                .unwrap();
            writeln!(file, "{phase}").unwrap();
        }
    }

    pub(crate) fn finish_segment(&self, serial: &str) {
        self.end_segment(serial, 0);
    }

    fn end_segment(&self, serial: &str, code: i32) {
        if let Some((_, done)) = self.state.lock().unwrap().active.remove(serial) {
            let _ = done.send(code);
        }
    }

    pub(crate) fn active(&self, serial: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .active
            .get(serial)
            .map(|(path, _)| path.clone())
    }

    pub(crate) async fn wait_for_segment(&self, serial: &str, seq: i32) {
        timeout(Duration::from_secs(3), async {
            while !self
                .active(serial)
                .is_some_and(|path| path.ends_with(&format!("_{seq}.mp4")))
            {
                sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("未启动目标分段");
    }

    fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.state.lock().unwrap().calls.clone()
    }
}

impl Adb for FakeAdb {
    type Process = FakeProcess;

    fn spawn(&self, serial: &str, args: &[&OsStr]) -> io::Result<FakeProcess> {
        let args: Vec<_> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        self.state.lock().unwrap().calls.push((
            serial.into(),
            args.iter().map(|arg| (*arg).into()).collect(),
        ));
        match args.as_slice() {
            [
                "shell",
                "exec",
                "screenrecord",
                "--time-limit",
                "170",
                "--bit-rate",
                "2000000",
                remote,
            ] => {
                self.write_phase("start");
                if let Some(detail) = self.start_error {
                    return Err(io::Error::other(detail));
                }
                let (done, wait) = oneshot::channel();
                let mut state = self.state.lock().unwrap();
                state.files.insert((serial.into(), (*remote).into()));
                assert!(
                    state
                        .active
                        .insert(serial.into(), ((*remote).into(), done))
                        .is_none()
                );
                Ok(FakeProcess::Recording(wait))
            }
            ["shell", "pkill", "-2", "-f", pattern] => {
                let remote = self.active(serial).unwrap();
                assert!(pattern.contains(&remote.replace('.', "[.]")));
                self.write_phase("stop");
                if !self.ignore_interrupt {
                    self.finish_segment(serial);
                }
                Ok(FakeProcess::Immediate(0))
            }
            ["pull", remote, local] => {
                assert!(self.active(serial).is_none(), "不能拉还在写入的分段");
                assert!(
                    self.state
                        .lock()
                        .unwrap()
                        .files
                        .contains(&(serial.into(), (*remote).into()))
                );
                if let Some(detail) = self.pull_error {
                    std::fs::write(local, &FAKE_MP4[..6])?;
                    return Err(io::Error::other(detail));
                }
                std::fs::write(local, FAKE_MP4)?;
                Ok(FakeProcess::Immediate(0))
            }
            ["shell", "rm", "-f", remote] => {
                self.state
                    .lock()
                    .unwrap()
                    .files
                    .remove(&(serial.into(), (*remote).into()));
                Ok(FakeProcess::Immediate(0))
            }
            ["shell", command] if command.starts_with("pkill -9 -f ") => {
                self.end_segment(serial, 137);
                Ok(FakeProcess::Immediate(0))
            }
            _ => panic!("未预期的 adb 命令：{args:?}"),
        }
    }
}

struct Fixture {
    dir: PathBuf,
    serial: String,
    id: Uuid,
}

impl Fixture {
    fn new() -> Self {
        let id = Uuid::new_v4();
        let dir = std::env::temp_dir()
            .join(format!("录屏测试-{id}"))
            .join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            dir,
            serial: format!("设备-{id}"),
            id,
        }
    }

    async fn start(&self, adb: FakeAdb, warn: impl Fn(String) + Send + Sync + 'static) -> Recorder {
        Recorder::start(
            &self.serial,
            &self.dir,
            adb,
            lock_device(&self.serial).await,
            warn,
            std::future::ready(()),
        )
        .await
    }

    fn index(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.dir.join("recording/index.json")).unwrap())
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.dir.parent().unwrap());
    }
}

#[tokio::test]
async fn 录屏命令按段执行并原子更新索引() {
    let fixture = Fixture::new();
    let adb = FakeAdb::default();
    let mut recorder = fixture
        .start(adb.clone(), |msg| panic!("不应有警告：{msg}"))
        .await;
    assert_eq!(fixture.index(), json!({"segments": []}));
    assert_eq!(
        adb.active(&fixture.serial),
        Some(format!("/sdcard/hafa_rec_{}_1.mp4", fixture.id))
    );
    adb.finish_segment(&fixture.serial);
    adb.wait_for_segment(&fixture.serial, 2).await;

    let first_snapshot = fixture.index();
    let mut old_index = std::fs::File::open(fixture.dir.join("recording/index.json")).unwrap();
    recorder.stop().await;

    let old_contents: serde_json::Value = serde_json::from_reader(&mut old_index).unwrap();
    assert_eq!(old_contents, first_snapshot, "已打开的旧索引不能被就地覆盖");
    let index = fixture.index();
    let segments = index["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 2);
    for (segment, seq) in segments.iter().zip([1, 2]) {
        let name = format!("rec_{seq:02}.mp4");
        assert_eq!(
            segment,
            &json!({
                "seq": seq, "file": name, "start_epoch_ms": segment["start_epoch_ms"], "duration_ms": segment["duration_ms"]
            })
        );
        assert!(segment["start_epoch_ms"].as_i64().unwrap() > 1_000_000_000_000);
        assert!(segment["duration_ms"].as_i64().unwrap() > 0);
        assert_eq!(
            std::fs::read(fixture.dir.join("recording").join(name)).unwrap(),
            FAKE_MP4
        );
    }
    let calls = adb.calls();
    assert!(calls.iter().all(|(serial, _)| serial == &fixture.serial));
    let commands: Vec<_> = calls
        .iter()
        .map(|(_, args)| {
            if args[0] == "shell" {
                args[1].as_str()
            } else {
                args[0].as_str()
            }
        })
        .collect();
    assert_eq!(
        commands,
        ["exec", "pull", "rm", "exec", "pkill", "pull", "rm"]
    );
    assert!(adb.state.lock().unwrap().files.is_empty());
    assert_eq!(
        std::fs::read_dir(fixture.dir.join("recording"))
            .unwrap()
            .count(),
        3
    );
}

#[tokio::test]
async fn 停止录屏仅影响当前设备且无进程遗留() {
    let first = Fixture::new();
    let second = Fixture::new();
    let adb = FakeAdb::default();
    let mut left = first.start(adb.clone(), |_| {}).await;
    let mut right = second.start(adb.clone(), |_| {}).await;
    left.stop().await;
    assert!(adb.active(&first.serial).is_none());
    assert!(adb.active(&second.serial).is_some());
    right.stop().await;
    let state = adb.state.lock().unwrap();
    assert!(state.active.is_empty() && state.files.is_empty());
}

#[tokio::test(start_paused = true)]
async fn 录屏中断信号超时后十秒内强制收尾且不发布损坏分段() {
    let fixture = Fixture::new();
    let adb = FakeAdb {
        ignore_interrupt: true,
        ..FakeAdb::default()
    };
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let capture = warnings.clone();
    let mut recorder = fixture
        .start(adb.clone(), move |msg| capture.lock().unwrap().push(msg))
        .await;
    let started = Instant::now();
    recorder.stop().await;
    assert!((GRACE_TIMEOUT..=Duration::from_secs(10)).contains(&started.elapsed()));
    assert!(adb.active(&fixture.serial).is_none());
    assert!(adb.state.lock().unwrap().files.is_empty());
    assert_eq!(fixture.index(), json!({"segments": []}));
    assert!(
        warnings
            .lock()
            .unwrap()
            .iter()
            .any(|msg| msg.contains("未正常结束"))
    );
    assert!(!adb.calls().iter().any(|(_, args)| args[0] == "pull"));
}

#[tokio::test]
async fn 录屏拉取失败重试三次只告警且不保留部分文件() {
    let fixture = Fixture::new();
    let mut adb = FakeAdb::default();
    const RAW_DETAIL: &str = "sample-recorder@hafa.invalid sample-password!";
    adb.pull_error = Some(RAW_DETAIL);
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let capture = warnings.clone();
    let mut recorder = fixture
        .start(adb.clone(), move |msg| capture.lock().unwrap().push(msg))
        .await;
    recorder.stop().await;
    assert_eq!(
        adb.calls()
            .iter()
            .filter(|(_, args)| args[0] == "pull")
            .count(),
        4
    );
    let warnings = warnings.lock().unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains("拉取录屏分段失败，已重试 3 次") && !warnings[0].contains(RAW_DETAIL)
    );
    assert_eq!(fixture.index(), json!({"segments": []}));
    assert!(adb.state.lock().unwrap().files.is_empty());
    assert_eq!(
        std::fs::read_dir(fixture.dir.join("recording"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn 取消时停止录屏并在下一次登录前完成收尾() {
    let fixture = Fixture::new();
    let adb = FakeAdb::default();
    let serial = fixture.serial.clone();
    let dir = fixture.dir.clone();
    let fake = adb.clone();
    let execution = tokio::spawn(async move {
        let _recorder = Recorder::start(
            &serial,
            &dir,
            fake,
            lock_device(&serial).await,
            |_| {},
            std::future::ready(()),
        )
        .await;
        std::future::pending::<()>().await;
    });
    adb.wait_for_segment(&fixture.serial, 1).await;
    execution.abort();
    assert!(execution.await.unwrap_err().is_cancelled());
    let _device = timeout(Duration::from_secs(3), lock_device(&fixture.serial))
        .await
        .unwrap();
    assert!(adb.active(&fixture.serial).is_none());
    assert_eq!(fixture.index()["segments"][0]["file"], "rec_01.mp4");
}
