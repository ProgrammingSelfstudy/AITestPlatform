//! 判定员的截图和步骤文字清单与取证工具。

use std::collections::{BTreeSet, HashMap};
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const NO_IMAGE: &str = "没有可用的清单内截图";
const NO_STEP_TEXT: &str = "没有可用的步骤文字";
pub const IMAGE_TOOL: &str = "mcp__evidence__image";
pub const STEP_TEXT_TOOL: &str = "mcp__evidence__step_text";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Before,
    Observe,
    After,
    Final,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Before => "操作前",
            Self::Observe => "操作前观察（有编号遮挡）",
            Self::After => "操作后原图",
            Self::Final => "终态原图",
        }
    }
}

fn shot_name(name: &str) -> Option<(&str, Phase)> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mut parts = name.strip_suffix(".png")?.split('_');
    let index = parts.next().filter(|s| digits(s))?;
    let phase = match parts.next()? {
        "before" => Phase::Before,
        "observe" => Phase::Observe,
        "after" => Phase::After,
        "final" => Phase::Final,
        _ => return None,
    };
    if parts.next().is_some_and(|s| !digits(s)) || parts.next().is_some() {
        return None;
    }
    let trimmed = index.trim_start_matches('0');
    Some((if trimmed.is_empty() { "0" } else { trimmed }, phase))
}

fn open_root(run_dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(run_dir.canonicalize()?)
}

// 以已打开的目录为根逐级打开，拒绝符号链接，即使检查清单后文件被换掉。
fn open_at(dir: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // 安全性：目录描述符和以 NUL 结尾的文件名在调用期间有效。
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600 as libc::c_int,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // 安全性：openat 成功返回一个由此处接管的独立描述符。
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_regular(dir: &File, name: &CStr) -> io::Result<File> {
    // 非阻塞打开可避免清单中的文件被替换成管道时挂住服务。
    let file = open_at(dir, name, libc::O_RDONLY | libc::O_NONBLOCK)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "证据文件必须是普通文件",
        ))
    }
}

fn open_png(shots: &File, name: &str) -> Option<File> {
    let mut file = open_regular(shots, &CString::new(name).ok()?).ok()?;
    let mut header = [0; PNG_MAGIC.len()];
    file.read_exact(&mut header).ok()?;
    (header == *PNG_MAGIC).then_some(file)
}

struct DirStream(*mut libc::DIR);

impl Drop for DirStream {
    fn drop(&mut self) {
        // 安全性：这是 fdopendir 返回的目录流，且仅在此处关闭。
        unsafe { libc::closedir(self.0) };
    }
}

fn shot_names(shots: &File) -> io::Result<Vec<String>> {
    let fd = shots.try_clone()?.into_raw_fd();
    // 安全性：fd 是独立的描述符，成功时交给目录流关闭。
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // 安全性：fdopendir 失败时仍由调用方关闭 fd。
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let stream = DirStream(stream);
    let mut names = Vec::new();
    loop {
        // 安全性：目录流有效且只在此处访问。目录项在下一次 readdir 前有效。
        let Some(entry) = (unsafe { libc::readdir(stream.0).as_ref() }) else {
            break;
        };
        // 安全性：readdir 保证 d_name 以 NUL 结尾。
        if let Ok(name) = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) }.to_str() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

#[derive(Deserialize)]
struct TraceStepText {
    idx: u32,
    #[serde(default)]
    action: String,
    target: Option<String>,
    #[serde(default)]
    after_labels: Vec<String>,
    #[serde(default)]
    new_labels: Vec<String>,
}

impl TraceStepText {
    fn summary(&self) -> String {
        let action = self.action.replace(['\r', '\n'], " / ");
        let target: String = self
            .target
            .as_deref()
            .unwrap_or_default()
            .replace(['\r', '\n'], " / ")
            .chars()
            .take(70)
            .collect();
        format!("{action} {target}").trim().to_owned()
    }
}

fn trace_steps(root: &File) -> impl Iterator<Item = TraceStepText> {
    #[derive(Deserialize)]
    struct Steps {
        steps: Vec<Value>,
    }

    open_regular(root, c"trace.json")
        .ok()
        .and_then(|f| serde_json::from_reader::<_, Steps>(f).ok())
        .map(|trace| trace.steps)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|step| serde_json::from_value(step).ok())
}

fn scan(root: &File) -> Vec<(String, String)> {
    let Ok(shots) = open_at(root, c"shots", libc::O_RDONLY | libc::O_DIRECTORY) else {
        return vec![];
    };
    let context: HashMap<_, _> = trace_steps(root)
        .map(|step| (step.idx, step.summary()))
        .collect();
    let mut entries = Vec::new();
    for name in shot_names(&shots).unwrap_or_default() {
        let Some((index, phase)) = shot_name(&name) else {
            continue;
        };
        if open_png(&shots, &name).is_none() {
            continue;
        }
        let detail = index
            .parse::<u32>()
            .ok()
            .and_then(|i| context.get(&i))
            .map_or("", String::as_str);
        let label = format!("步骤 {index} · {} {detail}", phase.label())
            .trim_end()
            .to_owned();
        entries.push((index.to_owned(), phase, name, label));
    }
    // 数字去掉前导零后按长度和字面排序，同阶段按文件名排序。
    entries
        .sort_unstable_by(|a, b| (a.0.len(), &a.0, a.1, &a.2).cmp(&(b.0.len(), &b.0, b.1, &b.2)));
    entries
        .into_iter()
        .map(|(_, _, name, label)| (name, label))
        .collect()
}

/// 读取本次运行可供判定员查看的截图名称和标签。
pub fn catalog(run_dir: &Path) -> Vec<(String, String)> {
    open_root(run_dir).map_or_else(|_| vec![], |root| scan(&root))
}

/// 按步骤号列出记录了动作后页面文字的步骤及其摘要。
pub fn step_text_catalog(run_dir: &Path) -> Vec<(u32, String)> {
    let Ok(root) = open_root(run_dir) else {
        return vec![];
    };
    let mut entries: Vec<_> = trace_steps(&root)
        .filter(|step| !step.after_labels.is_empty())
        .map(|step| {
            let new = if step.new_labels.is_empty() {
                "无新出现的文字"
            } else {
                "有新出现的文字"
            };
            (step.idx, format!("{} · {new}", step.summary()))
        })
        .collect();
    entries.sort_unstable_by_key(|(idx, _)| *idx);
    entries
}

fn write_served(root: &File, served: &BTreeSet<String>) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp_name = CString::new(format!(
        ".served-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
    .expect("临时文件名不含空字符");
    let temp = open_at(
        root,
        &temp_name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
    )?;
    let result = (|| {
        serde_json::to_writer(&temp, served).map_err(io::Error::other)?;
        temp.sync_all()?;
        // 安全性：两个文件名和目录描述符都有效，且在同一目录内原子替换。
        let rc = unsafe {
            libc::renameat(
                root.as_raw_fd(),
                temp_name.as_ptr(),
                root.as_raw_fd(),
                c"served.json".as_ptr(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })();
    if result.is_err() {
        // 安全性：删除自己刚才以 O_EXCL 创建的临时文件。
        unsafe { libc::unlinkat(root.as_raw_fd(), temp_name.as_ptr(), 0) };
    }
    result
}

struct Evidence {
    root: File,
    names: BTreeSet<String>,
}

fn record_served(root: &File, reference: String) -> io::Result<()> {
    // 两种证据共用一把锁，允许并发请求和重连累加同一份记录。文字取证可以没有 shots 目录。
    // Judge 持有轨迹目录锁，因此使用单独的文件锁保护查看记录。
    let lock = open_at(
        root,
        c".served.lock",
        libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK,
    )?;
    if !lock.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "查看记录的锁必须是普通文件",
        ));
    }
    // 安全性：文件描述符由 lock 持有，函数返回时关闭并释放锁。
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut served: BTreeSet<String> = open_regular(root, c"served.json")
        .ok()
        .and_then(|f| serde_json::from_reader(f).ok())
        .unwrap_or_default();
    if served.insert(reference) {
        write_served(root, &served)?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct EvidenceServer(Arc<Evidence>);

impl EvidenceServer {
    pub fn new(run_dir: &Path) -> io::Result<Self> {
        let root = open_root(run_dir)?;
        let names = scan(&root).into_iter().map(|(name, _)| name).collect();
        Ok(Self(Arc::new(Evidence { root, names })))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ImageArgs {
    /// 截图清单中的文件名。
    pub name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct StepTextArgs {
    /// 轨迹中的整数步骤号，例如 8。
    pub step: i64,
}

#[tool_router(server_handler)]
impl EvidenceServer {
    #[tool(description = "查看本次证据索引中的一张截图。name 必须是清单中的文件名，不接受路径。")]
    pub async fn image(
        &self,
        Parameters(ImageArgs { name }): Parameters<ImageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let error = || CallToolResult::error(vec![ContentBlock::text(NO_IMAGE)]);
        let evidence = &self.0;
        if name.contains('/') || name.contains("..") || !evidence.names.contains(&name) {
            return Ok(error());
        }
        let Ok(shots) = open_at(&evidence.root, c"shots", libc::O_RDONLY | libc::O_DIRECTORY)
        else {
            return Ok(error());
        };
        let Some(mut file) = open_png(&shots, &name) else {
            return Ok(error());
        };
        let mut bytes = PNG_MAGIC.to_vec();
        if file.read_to_end(&mut bytes).is_err() {
            return Ok(error());
        }
        if let Err(e) = record_served(&evidence.root, name) {
            tracing::warn!("写入截图查看记录失败：{e}");
            return Ok(error());
        }
        Ok(CallToolResult::success(vec![ContentBlock::image(
            base64::engine::general_purpose::STANDARD.encode(bytes),
            "image/png",
        )]))
    }

    #[tool(
        description = "按步骤号查看本次轨迹记录的动作、目标、动作后页面文字与新出现的文字。页面文字来自手机控件树，已脱敏。"
    )]
    pub async fn step_text(
        &self,
        Parameters(StepTextArgs { step }): Parameters<StepTextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let error = || CallToolResult::error(vec![ContentBlock::text(NO_STEP_TEXT)]);
        let root = &self.0.root;
        let Ok(idx) = u32::try_from(step) else {
            return Ok(error());
        };
        let Some(entry) = trace_steps(root).find(|entry| entry.idx == idx) else {
            return Ok(error());
        };
        if let Err(e) = record_served(root, format!("step:{idx}")) {
            tracing::warn!("写入步骤文字查看记录失败：{e}");
            return Ok(error());
        }
        Ok(CallToolResult::structured(json!({
            "action": entry.action,
            "target": entry.target,
            "after_labels": entry.after_labels,
            "new_labels": entry.new_labels,
        })))
    }
}
