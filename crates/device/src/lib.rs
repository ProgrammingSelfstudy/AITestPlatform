//! 设备层（Android，经 adb）。第一期只做 Android，但对外按「设备」抽象，不把平台写死。
//!
//! 规则（来自技术验证）：
//! - Hafa 是 Flutter 应用，控件树里 `text` 为空、`resource-id` 基本没有，**可读信息全在 content-desc**；
//!   定位优先级：content-desc > 坐标 > XPath > text / resource-id；
//! - dump 偶尔返回 0 个节点，要重试，最多 3 次；
//! - 设备命令失败把**原始错误**原样带出去，重试不超过 3 次；
//! - content-desc 里的换行统一当作 " / " 比较。

pub mod api;

use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;

/// 重试上限（设备命令失败 / dump 为空）。项目规则：不超过 3 次。
pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("无法启动 adb：{0}")]
    Spawn(#[from] std::io::Error),
    /// 原样保留命令、退出码和 stderr，给用户看原始错误
    #[error("adb 命令失败（退出码 {code:?}）：{cmd}\n原始输出：{stderr}")]
    Command { cmd: String, code: Option<i32>, stderr: String },
    #[error("控件树解析失败：{0}")]
    Xml(String),
    #[error("连续 {attempts} 次 dump 都没有拿到控件节点。诊断：{diag}")]
    EmptyDump { attempts: u32, diag: String },
    /// 这台设备（或这个实现）不支持的操作，如非 ASCII 文字输入
    #[error("不支持：{0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, DeviceError>;

// ---------------------------------------------------------------- 控件

/// 屏幕坐标 `[left, top, right, bottom]`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Bounds {
    pub fn center(&self) -> (i32, i32) {
        ((self.left + self.right) / 2, (self.top + self.bottom) / 2)
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.left <= x && x <= self.right && self.top <= y && y <= self.bottom
    }

    /// 解析 uiautomator 的 `[0,0][720,1600]` 写法。
    pub fn parse(s: &str) -> Option<Self> {
        let nums: Vec<i32> = s
            .split(|c: char| !(c.is_ascii_digit() || c == '-'))
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect();
        match nums.as_slice() {
            [l, t, r, b] => Some(Self { left: *l, top: *t, right: *r, bottom: *b }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Element {
    /// content-desc，换行已替换为 " / "
    pub desc: String,
    pub text: String,
    pub class: String,
    pub resource_id: String,
    pub bounds: Bounds,
    pub clickable: bool,
    pub enabled: bool,
    pub scrollable: bool,
}

/// 状态栏（时间、网速、电量等）里的节点：底边不超过这个值。
/// 必须按 bottom 判断，不能按 top：页面标题的 top 可能只有 99。
pub const STATUS_BAR_BOTTOM: i32 = 90;

impl Element {
    pub fn in_status_bar(&self) -> bool {
        self.bounds.bottom <= STATUS_BAR_BOTTOM
    }

    /// 可读标签：优先 content-desc，没有再用 text。
    pub fn label(&self) -> &str {
        if !self.desc.is_empty() { &self.desc } else { &self.text }
    }
}

/// content-desc 里的换行统一当作 " / "。
pub fn norm(s: &str) -> String {
    s.replace('\n', " / ").trim().to_string()
}

/// 解析 `uiautomator dump` 的 XML。
pub fn parse_hierarchy(xml: &str) -> Result<Vec<Element>> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| DeviceError::Xml(e.to_string()))?;
    let flag = |n: &roxmltree::Node, k: &str| n.attribute(k) == Some("true");
    Ok(doc
        .descendants()
        .filter(|n| n.has_tag_name("node"))
        .filter_map(|n| {
            Some(Element {
                desc: norm(n.attribute("content-desc").unwrap_or("")),
                text: norm(n.attribute("text").unwrap_or("")),
                class: n.attribute("class").unwrap_or("").to_string(),
                resource_id: n.attribute("resource-id").unwrap_or("").to_string(),
                bounds: Bounds::parse(n.attribute("bounds")?)?,
                clickable: flag(&n, "clickable"),
                enabled: n.attribute("enabled") != Some("false"),
                scrollable: flag(&n, "scrollable"),
            })
        })
        .collect())
}

/// 按 content-desc 找元素。`exact=false` 为包含匹配；同时命中时优先可点击的。
pub fn find<'a>(els: &'a [Element], desc: &str, exact: bool) -> Option<&'a Element> {
    let want = norm(desc);
    let mut hits: Vec<&Element> = els
        .iter()
        .filter(|e| !e.label().is_empty())
        .filter(|e| if exact { e.label() == want } else { e.label().contains(&want) })
        .collect();
    hits.sort_by_key(|e| !e.clickable);
    hits.into_iter().next()
}

// ---------------------------------------------------------------- 设备列表

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub serial: String,
    /// `device` / `offline` / `unauthorized` 等 adb 原始状态
    pub state: String,
    pub model: Option<String>,
}

/// 解析 `adb devices -l` 的输出。
pub fn parse_devices(out: &str) -> Vec<DeviceEntry> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter(|l| !l.trim().is_empty() && !l.starts_with('*'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?.to_string();
            let state = it.next()?.to_string();
            let model = it.find_map(|p| p.strip_prefix("model:").map(str::to_string));
            Some(DeviceEntry { serial, state, model })
        })
        .collect()
}

/// 列出当前连接的设备。
pub async fn list_devices() -> Result<Vec<DeviceEntry>> {
    let out = run_adb(None, &["devices", "-l"]).await?;
    Ok(parse_devices(&String::from_utf8_lossy(&out)))
}

// ---------------------------------------------------------------- adb 调用

async fn run_adb(serial: Option<&str>, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("adb");
    if let Some(s) = serial {
        cmd.args(["-s", s]);
    }
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let out = cmd.output().await?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(DeviceError::Command {
            cmd: format!("adb {}{}", serial.map(|s| format!("-s {s} ")).unwrap_or_default(), args.join(" ")),
            code: out.status.code(),
            // 原始错误原样带出：stderr 为空时退回 stdout
            stderr: {
                let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if e.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { e }
            },
        })
    }
}

/// 带重试的 adb 调用：最多 `MAX_ATTEMPTS` 次，最后一次的原始错误原样返回。
async fn run_adb_retry(serial: &str, args: &[&str]) -> Result<Vec<u8>> {
    let mut last = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match run_adb(Some(serial), args).await {
            Ok(o) => return Ok(o),
            Err(e) => {
                tracing::warn!(attempt, "adb 命令失败：{e}");
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(last.expect("至少执行过一次"))
}

const DUMP_BACKOFF_MS: [u64; MAX_ATTEMPTS as usize] = [800, 1500, 2500];

fn dump_preview(raw: &[u8]) -> String {
    const SAFE_PARTS: &[&str] = &[
        "ERROR: null root node returned by UiTestAutomationBridge.",
        "ERROR: could not get idle state.",
        "ERROR: ",
        "UI hierchary dumped to: /dev/tty",
        "UI hierarchy dumped to: /dev/tty",
        "<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>",
        "<?xml",
        "<hierarchy rotation=\"0\"", "<hierarchy rotation=\"1\"",
        "<hierarchy rotation=\"2\"", "<hierarchy rotation=\"3\"",
        "<hierarchy", "</hierarchy>", "<node", "/>", ">",
        " ", "\t", "\u{fffd}",
    ];
    let prefix = String::from_utf8_lossy(&raw[..raw.len().min(200)]);
    if prefix.trim().is_empty() {
        return "（空）".into();
    }
    prefix.replace("\r\n", "\n").split(['\r', '\n']).map(|line| {
        let mut rest = line;
        while let Some(suffix) = SAFE_PARTS.iter().find_map(|part| rest.strip_prefix(*part)) {
            rest = suffix;
        }
        if rest.is_empty() { line.to_string() } else { format!("{}[已脱敏]", &line[..line.len() - rest.len()]) }
    }).collect::<Vec<_>>().join(" ")
}

fn focus_preview(raw: &[u8]) -> Option<String> {
    let windows = String::from_utf8_lossy(raw);
    let value = windows.lines().find_map(|line| line.trim().strip_prefix("mCurrentFocus="))?;
    if value == "null" {
        return Some("mCurrentFocus=null".into());
    }
    let mut parts = value.strip_prefix("Window{")?.strip_suffix('}')?.splitn(3, ' ');
    let id = parts.next().filter(|id| !id.is_empty() && id.len() <= 8 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))?;
    let user = parts.next().filter(|user| user.strip_prefix('u').is_some_and(|id| id.parse::<u32>().is_ok()))?;
    let title = parts.next()?;
    let (title, suffix) = title.strip_suffix(" EXITING").map_or((title, ""), |title| (title, " EXITING"));
    let qualified_name = |name: &str| name.split('.').all(|part| {
        part.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    });
    let component = title.split_once('/').is_some_and(|(package, activity)| {
        package.contains('.') && qualified_name(package)
            && activity.contains('.') && qualified_name(activity.strip_prefix('.').unwrap_or(activity))
    });
    let title = if component || matches!(title, "NotificationShade" | "StatusBar" | "Keyguard" | "InputMethod") {
        title
    } else {
        "[已脱敏]"
    };
    Some(format!("mCurrentFocus=Window{{{id} {user} {title}{suffix}}}"))
}

async fn dump_with<F: Future<Output = Result<Vec<u8>>>>(mut adb: impl FnMut(&'static [&'static str]) -> F) -> Result<Vec<Element>> {
    let mut delays = DUMP_BACKOFF_MS.into_iter().take(MAX_ATTEMPTS as usize - 1);
    let mut attempt = 1;
    let raw = loop {
        let result = adb(&["exec-out", "uiautomator", "dump", "/dev/tty"]).await;
        match &result {
            Ok(raw) => {
                let text = String::from_utf8_lossy(raw);
                if let Some(end) = text.find("</hierarchy>") {
                    let xml = &text[..end + "</hierarchy>".len()];
                    let els = parse_hierarchy(&xml[xml.find("<?xml").unwrap_or(0)..])?;
                    if !els.is_empty() {
                        return Ok(els);
                    }
                }
            }
            Err(err) => tracing::warn!(attempt, "adb 命令失败：{err}"),
        }
        let Some(delay) = delays.next() else { break result? };
        if result.is_ok() {
            tracing::warn!(attempt, "dump 没有拿到控件节点，重试");
        }
        tokio::time::sleep(Duration::from_millis(delay)).await;
        attempt += 1;
    };
    let focus = match tokio::time::timeout(Duration::from_secs(2), adb(&["shell", "dumpsys window"])).await {
        Ok(Ok(windows)) => focus_preview(&windows),
        _ => None,
    }.unwrap_or_else(|| "mCurrentFocus=诊断不可用".into());
    let diag = format!("adb 输出前 200 字节：{}；{focus}", dump_preview(&raw));
    Err(DeviceError::EmptyDump { attempts: MAX_ATTEMPTS, diag })
}

/// 一台 Android 设备。
#[derive(Debug, Clone)]
pub struct Android {
    pub serial: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub model: Option<String>,
    pub os_version: Option<String>,
    pub screen_w: Option<u32>,
    pub screen_h: Option<u32>,
}

/// `dumpsys power` 里是否处于休眠。
pub fn is_asleep(dumpsys_power: &str) -> bool {
    dumpsys_power.contains("mWakefulness=Asleep")
}

/// `dumpsys window` 里是否正显示锁屏。
pub fn keyguard_showing(dumpsys_window: &str) -> bool {
    dumpsys_window.contains("isKeyguardShowing=true")
}

/// 单引号包起来，里面的单引号写成 `'\''`：交给设备上的 shell 解析时原样保留。
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 把要输入的文字转成 `input text` 能接受的写法：空格写成 `%s`。
/// 不是 ASCII、含控制字符或含 `%` 的文字没法可靠输入，返回 `Unsupported`（调用方要如实报告，不能静默吞掉）。
pub fn escape_input_text(text: &str) -> Result<String> {
    if let Some(c) = text.chars().find(|c| !c.is_ascii() || c.is_ascii_control() || *c == '%') {
        return Err(DeviceError::Unsupported(format!("`adb input text` 无法输入字符 {c:?}（只支持不含 % 的 ASCII 可见字符）")));
    }
    Ok(text.replace(' ', "%s"))
}

/// 解析 `wm size` 的 `Physical size: 720x1600`；有 Override size 时以实际生效的为准。
pub fn parse_wm_size(out: &str) -> Option<(u32, u32)> {
    let pick = |prefix: &str| {
        out.lines().find_map(|l| l.trim().strip_prefix(prefix).map(str::trim)).and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
    };
    pick("Override size:").or_else(|| pick("Physical size:"))
}

impl Android {
    pub fn new(serial: impl Into<String>) -> Self {
        Self { serial: serial.into() }
    }

    pub async fn shell(&self, cmd: &str) -> Result<String> {
        let out = run_adb_retry(&self.serial, &["shell", cmd]).await?;
        Ok(String::from_utf8_lossy(&out).to_string())
    }

    pub async fn info(&self) -> Result<Info> {
        let prop = |s: String| Some(s.trim().to_string()).filter(|v| !v.is_empty());
        let model = prop(self.shell("getprop ro.product.model").await?);
        let os_version = prop(self.shell("getprop ro.build.version.release").await?);
        let size = parse_wm_size(&self.shell("wm size").await?);
        Ok(Info { model, os_version, screen_w: size.map(|s| s.0), screen_h: size.map(|s| s.1) })
    }

    /// 截图（PNG 字节）。
    pub async fn screenshot_png(&self) -> Result<Vec<u8>> {
        run_adb_retry(&self.serial, &["exec-out", "screencap", "-p"]).await
    }

    /// dump 控件树。拿到 0 个节点时重试，最多 `MAX_ATTEMPTS` 次，仍为空返回 `EmptyDump`。
    pub async fn dump(&self) -> Result<Vec<Element>> {
        dump_with(|args| run_adb(Some(&self.serial), args)).await
    }

    pub async fn tap(&self, x: i32, y: i32) -> Result<()> {
        self.shell(&format!("input tap {x} {y}")).await.map(|_| ())
    }

    pub async fn swipe(&self, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<()> {
        self.shell(&format!("input swipe {x1} {y1} {x2} {y2} {ms}")).await.map(|_| ())
    }

    pub async fn back(&self) -> Result<()> {
        self.shell("input keyevent 4").await.map(|_| ())
    }

    /// 往当前聚焦的输入框输入文字。`clear` 为真时先清空（房间名等字段会预填旧值）。
    /// 走 `adb shell input text`，**只支持 ASCII**，且不含 `%`；别的文字返回 `Unsupported`，不静默吞掉。
    /// 输入完会收起软键盘：键盘会挡住下方按钮，节点还会塞满元素列表。
    pub async fn type_text(&self, text: &str, clear: bool) -> Result<()> {
        let escaped = escape_input_text(text)?;
        if clear {
            // 光标移到末尾，再连发退格；Flutter 输入框里自带的清空不生效
            self.shell("input keyevent KEYCODE_MOVE_END").await?;
            self.shell(&format!("input keyevent {}", vec!["KEYCODE_DEL"; 60].join(" "))).await?;
        }
        self.shell(&format!("input text {}", shell_quote(&escaped))).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        if self.shell("dumpsys input_method").await?.contains("mInputShown=true") {
            self.back().await?;
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        Ok(())
    }

    /// 把本机文件推到设备上。
    pub async fn push_file(&self, local: &std::path::Path, remote: &str) -> Result<()> {
        let l = local.to_string_lossy();
        run_adb_retry(&self.serial, &["push", &l, remote]).await.map(|_| ())
    }

    /// 启动应用（包名 / Activity 来自配置，不写死）。
    pub async fn launch_app(&self, package: &str, activity: &str) -> Result<()> {
        self.shell(&format!("am start -n {package}/{activity}")).await.map(|_| ())
    }

    /// 强制停止应用。
    pub async fn stop_app(&self, package: &str) -> Result<()> {
        self.shell(&format!("am force-stop {package}")).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<hierarchy rotation="0">
  <node index="0" text="" resource-id="" class="android.view.View" content-desc="Music(0/50)" clickable="false" enabled="true" scrollable="false" bounds="[0,100][720,200]" />
  <node index="1" text="" resource-id="" class="android.view.View" content-desc="tst_normal_01&#10;00:30" clickable="true" enabled="true" scrollable="false" bounds="[22,300][698,400]" />
  <node index="2" text="" resource-id="" class="android.view.View" content-desc="Add Music" clickable="true" enabled="true" scrollable="false" bounds="[22,1487][698,1577]" />
  <node index="3" text="" resource-id="" class="android.view.View" content-desc="Add Music" clickable="false" enabled="true" scrollable="false" bounds="[0,0][10,10]" />
</hierarchy>"#;

    #[test]
    fn 输入文字的转义() {
        assert_eq!(escape_input_text("abc123").unwrap(), "abc123");
        assert_eq!(escape_input_text("a b").unwrap(), "a%sb", "空格要写成 %s");
        for bad in ["中文", "100%", "tab\there", "换行\n"] {
            assert!(matches!(escape_input_text(bad), Err(DeviceError::Unsupported(_))), "{bad:?} 不能静默吞掉，必须明确报不支持");
        }
    }

    #[test]
    fn shell引号() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'", "单引号要拆开转义，否则会提前结束引用");
        // 危险字符被引号包住后不会被 shell 当成命令
        assert_eq!(shell_quote("a;rm -rf /$(x)`y`&|<>"), "'a;rm -rf /$(x)`y`&|<>'");
    }

    #[test]
    fn 休眠和锁屏的判断() {
        assert!(is_asleep("Power Manager State:\n  mWakefulness=Asleep\n"));
        assert!(!is_asleep("mWakefulness=Awake"));
        assert!(keyguard_showing("  mKeyguardController:\n    isKeyguardShowing=true\n"));
        assert!(!keyguard_showing("isKeyguardShowing=false"));
        assert!(!keyguard_showing(""), "读不到信息时不能当成锁屏");
    }

    #[test]
    fn 状态栏判断按底边() {
        let mk = |t, b| Element {
            desc: "x".into(), text: String::new(), class: String::new(), resource_id: String::new(),
            bounds: Bounds { left: 0, top: t, right: 100, bottom: b },
            clickable: false, enabled: true, scrollable: false,
        };
        assert!(mk(0, 80).in_status_bar());
        assert!(mk(0, 90).in_status_bar(), "底边正好 90 算状态栏");
        assert!(!mk(99, 160).in_status_bar(), "页面标题 top=99 不能被当成状态栏");
        assert!(!mk(40, 120).in_status_bar(), "按 bottom 判断：top 在状态栏里但底边超出的不算");
    }

    #[test]
    fn 解析bounds() {
        assert_eq!(Bounds::parse("[22,1487][698,1577]"), Some(Bounds { left: 22, top: 1487, right: 698, bottom: 1577 }));
        assert_eq!(Bounds::parse("坏数据"), None);
        assert_eq!(Bounds::parse("[1,2][3]"), None);
    }

    #[test]
    fn bounds中心和包含() {
        let b = Bounds::parse("[22,1487][698,1577]").unwrap();
        assert_eq!(b.center(), (360, 1532));
        assert!(b.contains(360, 1532) && !b.contains(5, 300));
    }

    #[test]
    fn 解析控件树并规整换行() {
        let els = parse_hierarchy(XML).unwrap();
        assert_eq!(els.len(), 4);
        assert_eq!(els[1].desc, "tst_normal_01 / 00:30", "content-desc 里的换行要当作 ' / '");
        assert!(els[1].clickable && !els[0].clickable);
    }

    #[test]
    fn 按content_desc查找并优先可点击() {
        let els = parse_hierarchy(XML).unwrap();
        let hit = find(&els, "Add Music", true).unwrap();
        assert!(hit.clickable, "同名时优先可点击的那个");
        assert_eq!(hit.bounds.center(), (360, 1532));
        assert!(find(&els, "Music(", false).is_some(), "exact=false 为包含匹配");
        assert!(find(&els, "不存在", true).is_none());
        assert!(find(&els, "tst_normal_01\n00:30", true).is_some(), "查询串里的换行也要规整");
    }

    #[test]
    fn 空控件树解析为空列表() {
        let els = parse_hierarchy("<?xml version='1.0'?><hierarchy rotation=\"0\"></hierarchy>").unwrap();
        assert!(els.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn 连续三次空控件树逐次延长等待并保留最后诊断() {
        let started = tokio::time::Instant::now();
        let mut calls = vec![];
        let mut last = b"ERROR: could not get idle state.\r\n".to_vec();
        last.resize(199, b' ');
        last.extend_from_slice("中\nUI hierchary dumped to: /dev/tty".as_bytes());
        let err = dump_with(|args| {
            calls.push((args, started.elapsed().as_millis()));
            let raw = match calls.len() {
                1 => b"ERROR: null root node returned by UiTestAutomationBridge.".to_vec(),
                2 => b"<hierarchy></hierarchy>".to_vec(),
                3 => last.clone(),
                4 => b"  mCurrentFocus=Window{abc u0 com.example/.MusicActivity}\n  mFocusedApp=other".to_vec(),
                _ => panic!("不能再调用 adb"),
            };
            std::future::ready(Ok(raw))
        }).await.unwrap_err();
        assert_eq!(calls, [
            (&["exec-out", "uiautomator", "dump", "/dev/tty"][..], 0),
            (&["exec-out", "uiautomator", "dump", "/dev/tty"][..], 800),
            (&["exec-out", "uiautomator", "dump", "/dev/tty"][..], 2300),
            (&["shell", "dumpsys window"][..], 2300),
        ]);
        assert!(matches!(err, DeviceError::EmptyDump { attempts: 3, .. }));
        let msg = err.to_string();
        assert!(msg.contains("连续 3 次 dump 都没有拿到控件节点") && msg.contains("诊断："), "{msg}");
        assert!(msg.contains("ERROR: could not get idle state. ") && msg.contains("�；mCurrentFocus=Window{abc u0 com.example/.MusicActivity}"), "{msg}");
        for absent in ["null root node", "<hierarchy>", "UI hierchary", "mFocusedApp", "中", "\n", "\r"] {
            assert!(!msg.contains(absent), "不应含 {absent:?}：{msg}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn 焦点诊断失败或超时仍返回空控件树错误() {
        for mode in ["失败", "无焦点行", "超时"] {
            let err = dump_with(|args| async move {
                if args[0] == "exec-out" {
                    return Ok(b"<hierarchy></hierarchy>".to_vec());
                }
                match mode {
                    "失败" => Err(DeviceError::Unsupported("焦点命令内部错误".into())),
                    "无焦点行" => Ok(b"mFocusedApp=other".to_vec()),
                    _ => std::future::pending().await,
                }
            }).await.unwrap_err();
            assert!(matches!(err, DeviceError::EmptyDump { attempts: 3, .. }));
            let msg = err.to_string();
            assert!(msg.contains("<hierarchy></hierarchy>") && msg.contains("mCurrentFocus=诊断不可用"), "{msg}");
            assert!(!msg.contains("焦点命令内部错误"), "{msg}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn 空控件树与命令失败共用三次上限且保留原错误() {
        let mut calls = 0;
        let err = dump_with(|args| {
            calls += 1;
            assert_eq!(args[1], "uiautomator", "命令失败不取焦点诊断");
            let reply = if calls == 1 { Ok(vec![]) } else {
                Err(DeviceError::Command { cmd: "adb exec-out uiautomator".into(), code: Some(1), stderr: "device offline".into() })
            };
            std::future::ready(reply)
        }).await.unwrap_err();
        assert_eq!(calls, 3);
        assert!(matches!(err, DeviceError::Command { stderr, .. } if stderr == "device offline"));
    }

    #[test]
    fn 控件属性和自由窗口标题不进诊断() {
        let preview = dump_preview(br#"<hierarchy rotation="0"><node text="tester@example.com" content-desc="s3cret"/></hierarchy>"#);
        let focus = focus_preview(b"  mCurrentFocus=Window{abc u0 tester@example.com s3cret}\n").unwrap();
        assert_eq!(preview, "<hierarchy rotation=\"0\"><node [已脱敏]");
        assert_eq!(focus, "mCurrentFocus=Window{abc u0 [已脱敏]}");
    }

    #[test]
    fn 坏xml返回错误() {
        assert!(matches!(parse_hierarchy("<hierarchy><node"), Err(DeviceError::Xml(_))));
    }

    #[test]
    fn 解析设备列表() {
        let out = "List of devices attached\n1263040493010797\tdevice product:X6531 model:Infinix_X6531 device:X6531 transport_id:1\n15064060\tdevice product:CPH2631 model:CPH2631\nabc\tunauthorized\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].serial, "1263040493010797");
        assert_eq!(d[0].model.as_deref(), Some("Infinix_X6531"));
        assert_eq!(d[2].state, "unauthorized");
    }

    #[test]
    fn 守护进程启动提示行不算设备() {
        let out = "* daemon not running; starting now at tcp:5037\n* daemon started successfully\nList of devices attached\nabc\tdevice\n";
        assert_eq!(parse_devices(out).len(), 1);
    }

    #[test]
    fn 解析屏幕尺寸() {
        assert_eq!(parse_wm_size("Physical size: 720x1604\n"), Some((720, 1604)));
        assert_eq!(parse_wm_size("Physical size: 720x1600\nOverride size: 540x1200\n"), Some((540, 1200)));
        assert_eq!(parse_wm_size("乱码"), None);
    }

    #[test]
    fn adb失败时原样带出错误() {
        // 用不存在的设备序列号触发真实的 adb 报错，验证原始错误不被吞掉
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err = rt.block_on(async { Android::new("不存在的序列号").shell("echo hi").await }).unwrap_err();
        match err {
            DeviceError::Command { cmd, stderr, .. } => {
                assert!(cmd.contains("不存在的序列号"));
                assert!(!stderr.is_empty(), "stderr 必须保留 adb 的原始输出");
            }
            DeviceError::Spawn(_) => {} // 本机没有 adb 时也算通过：错误同样被原样带出
            other => panic!("期望 Command 错误，实际：{other:?}"),
        }
    }

    /// 真机：唤醒解锁。会把手机设成 USB 连接时常亮（和技术验证里的 setup 一样）。
    #[tokio::test]
    #[ignore]
    async fn 真机_唤醒并解锁() {
        let devices = list_devices().await.unwrap();
        let d = devices.iter().find(|d| d.state == "device").expect("没有在线设备");
        use crate::api::DeviceApi;
        let a = Android::new(d.serial.clone());
        let unlocked = a.ensure_awake().await.unwrap();
        println!("设备 {}：已解锁 = {unlocked}", d.serial);
        assert!(unlocked, "唤醒后仍显示锁屏：可能设了 PIN / 图案，需要人工");
    }

    /// 真机测试，需要手机连着：`cargo test -p device -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn 真机_设备信息_截图_dump() {
        let devices = list_devices().await.unwrap();
        let d = devices.iter().find(|d| d.state == "device").expect("没有在线设备");
        let a = Android::new(d.serial.clone());
        let info = a.info().await.unwrap();
        println!("设备 {}：{info:?}", d.serial);
        let png = a.screenshot_png().await.unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "截图不是 PNG");
        let els = a.dump().await.unwrap();
        println!("控件 {} 个，有 content-desc 的 {} 个", els.len(), els.iter().filter(|e| !e.desc.is_empty()).count());
        assert!(!els.is_empty());
    }
}
