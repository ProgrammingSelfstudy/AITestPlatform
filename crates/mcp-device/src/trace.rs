//! 执行轨迹：每一步落盘（崩溃 / 重启后能续跑），截图存文件，账号密码绝不进轨迹。
//!
//! 落盘是原子的（先写临时文件再改名），避免进程被杀时留下半个 JSON。
//! 脱敏：注册过的秘密值在写入任何文字之前统一替换成占位符（如 `{{account.password}}`）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 复用轨迹的占位符脱敏规则，供读到旧轨迹或拼接其他证据的调用方使用。
/// 长的先替换，避免短的把长的拆碎。
pub fn redact_text(text: &str, secrets: &[(String, String)]) -> String {
    let mut order: Vec<_> = secrets.iter().filter(|(value, _)| !value.is_empty()).collect();
    order.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
    order.into_iter().fold(text.to_owned(), |text, (value, placeholder)| {
        text.replace(value.as_str(), placeholder)
    })
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceStep {
    /// 第几步，从 1 开始
    pub idx: u32,
    /// 步骤开始的节点时间（epoch 毫秒）；旧轨迹未记录时为 0。
    #[serde(default)]
    pub at_ms: u64,
    pub ts_ms: u64,
    /// `tap` / `tap_xy` / `swipe` / `back` / `launch_app` / `restart_app` / `wait` / `need_human` ……
    pub action: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub degraded: bool,
    /// 目标元素的 content-desc（稳定的名字，脚本生成时用它定位）
    pub target: Option<String>,
    /// 目标元素前一个有文字的元素（输入框等没有稳定名字的元素靠它定位）
    pub prev_label: Option<String>,
    /// 动作的值（已脱敏）：等待秒数、人工说明等
    pub value: Option<String>,
    /// 点击的坐标，作为兜底
    pub point: Option<(i32, i32)>,
    /// 动作后的截图（相对 run 目录）
    pub shot: Option<String>,
    /// 动作后页面上的文字（脚本生成时据此写「等到页面出现 X」和断言）
    pub after_labels: Vec<String>,
    /// 动作后新出现的文字
    pub new_labels: Vec<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TraceFile {
    case: String,
    platform: String,
    model: String,
    started_ms: u64,
    finished_ms: Option<u64>,
    result: Option<String>,
    reason: Option<String>,
    evidence: Vec<String>,
    steps: Vec<TraceStep>,
}

pub struct Trace {
    dir: PathBuf,
    file: TraceFile,
    /// (秘密值, 占位符)
    secrets: Vec<(String, String)>,
}

impl Trace {
    /// 新建，或接着目录里已有的 `trace.json` 续跑。
    pub fn open(dir: impl Into<PathBuf>, case: &str, platform: &str, model: &str) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(dir.join("shots"))?;
        let path = dir.join("trace.json");
        let file = match std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<TraceFile>(&t).ok()) {
            Some(f) => f,
            None => TraceFile {
                case: case.into(),
                platform: platform.into(),
                model: model.into(),
                started_ms: now_ms(),
                ..Default::default()
            },
        };
        let t = Self { dir, file, secrets: vec![] };
        // 一次执行从一开始就有 trace.json（哪怕 0 步）：Judge 和页面要能分清「没有步骤」和「根本没跑起来」
        t.flush()?;
        Ok(t)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 续跑时已有多少步。
    pub fn step_count(&self) -> u32 {
        self.file.steps.len() as u32
    }

    pub(crate) fn counted_steps(&self) -> u32 {
        self.file.steps.iter().filter(|step| step.action != "observe").count() as u32
    }

    /// 登记一个需要脱敏的值；空值和太短的值（容易误伤正常文字）不登记。
    pub fn add_secret(&mut self, value: &str, placeholder: &str) {
        if value.chars().count() >= 3 {
            self.secrets.push((value.to_string(), placeholder.to_string()));
        }
    }

    /// 把文字里的秘密值替换成占位符。长的先替换，避免短的把长的拆碎。
    pub fn redact(&self, text: &str) -> String {
        redact_text(text, &self.secrets)
    }

    /// 存截图，返回相对 run 目录的路径，如 `shots/04_after.png`。
    pub fn save_shot(&self, idx: u32, kind: &str, bytes: &[u8]) -> std::io::Result<String> {
        let rel = format!("shots/{idx:02}_{kind}.png");
        std::fs::write(self.dir.join(&rel), bytes)?;
        Ok(rel)
    }

    pub fn add_step(&mut self, mut step: TraceStep) -> std::io::Result<()> {
        step.idx = self.file.steps.len() as u32 + 1;
        step.ts_ms = now_ms();
        step.value = step.value.map(|v| self.redact(&v));
        step.target = step.target.map(|v| self.redact(&v));
        step.prev_label = step.prev_label.map(|v| self.redact(&v));
        step.after_labels = step.after_labels.iter().map(|l| self.redact(l)).collect();
        step.new_labels = step.new_labels.iter().map(|l| self.redact(l)).collect();
        self.file.steps.push(step);
        self.flush()
    }

    pub fn finish(&mut self, result: &str, reason: &str, evidence: Vec<String>) -> std::io::Result<()> {
        self.file.finished_ms = Some(now_ms());
        self.file.result = Some(result.into());
        self.file.reason = Some(self.redact(reason));
        self.file.evidence = evidence;
        self.flush()
    }

    /// 原子落盘。
    pub fn flush(&self) -> std::io::Result<()> {
        let tmp = self.dir.join("trace.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.file).expect("轨迹可序列化"))?;
        std::fs::rename(tmp, self.dir.join("trace.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试一个独立目录。只用时间戳会让同一毫秒启动的并行测试撞名，所以加原子计数器。
    fn tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!("trace_test_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
    }

    fn step(action: &str) -> TraceStep {
        TraceStep { action: action.into(), ..Default::default() }
    }

    #[test]
    fn 打开时就落一个空轨迹() {
        let d = tmp();
        let t = Trace::open(&d, "TC-04", "android", "m").unwrap();
        assert_eq!(t.step_count(), 0);
        let f: TraceFile = serde_json::from_str(&std::fs::read_to_string(d.join("trace.json")).unwrap()).unwrap();
        assert_eq!((f.case.as_str(), f.steps.len(), f.result), ("TC-04", 0, None));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 每一步立刻落盘且编号递增() {
        let d = tmp();
        let mut t = Trace::open(&d, "TC-04", "android", "m").unwrap();
        t.add_step(step("tap")).unwrap();
        // 不调用 finish / flush，文件里也已经有这一步：进程被杀不会丢
        let text = std::fs::read_to_string(d.join("trace.json")).unwrap();
        assert!(text.contains("\"action\": \"tap\""), "{text}");
        t.add_step(step("back")).unwrap();
        assert_eq!(t.step_count(), 2);
        let back: TraceFile = serde_json::from_str(&std::fs::read_to_string(d.join("trace.json")).unwrap()).unwrap();
        assert_eq!(back.steps.iter().map(|s| s.idx).collect::<Vec<_>>(), [1, 2]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 旧轨迹没有开始时间仍可读取() {
        let mut old = serde_json::to_value(step("tap")).unwrap();
        old.as_object_mut().unwrap().remove("at_ms");
        assert_eq!(serde_json::from_value::<TraceStep>(old).unwrap().at_ms, 0);
    }

    #[test]
    fn 重新打开能续跑() {
        let d = tmp();
        {
            let mut t = Trace::open(&d, "TC-04", "android", "m").unwrap();
            t.add_step(step("tap")).unwrap();
            t.add_step(step("tap")).unwrap();
        }
        let mut t = Trace::open(&d, "TC-04", "android", "m").unwrap();
        assert_eq!(t.step_count(), 2, "续跑要接着已有步数");
        t.add_step(step("back")).unwrap();
        assert_eq!(t.step_count(), 3);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 密码绝不进轨迹文件() {
        let d = tmp();
        let mut t = Trace::open(&d, "TC-01", "android", "m").unwrap();
        t.add_secret("S3cretPassw0rd!", "{{account.password}}");
        t.add_secret("tester@example.com", "{{account.username}}");
        let mut s = step("input_text");
        s.value = Some("S3cretPassw0rd!".into());
        s.target = Some("Email tester@example.com".into());
        s.after_labels = vec!["已输入 S3cretPassw0rd!".into()];
        t.add_step(s).unwrap();
        t.finish("passed", "用 tester@example.com / S3cretPassw0rd! 登录成功", vec![]).unwrap();

        let text = std::fs::read_to_string(d.join("trace.json")).unwrap();
        assert!(!text.contains("S3cretPassw0rd!"), "密码泄露到轨迹里了：{text}");
        assert!(!text.contains("tester@example.com"), "账号泄露到轨迹里了：{text}");
        assert!(text.contains("{{account.password}}") && text.contains("{{account.username}}"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 脱敏优先替换更长的秘密() {
        let mut t = Trace::open(tmp(), "c", "android", "m").unwrap();
        t.add_secret("abc", "{{short}}");
        t.add_secret("abcdef", "{{long}}");
        assert_eq!(t.redact("x abcdef y"), "x {{long}} y", "短的不能把长的拆碎");
    }

    #[test]
    fn 太短的值不登记以免误伤正常文字() {
        let mut t = Trace::open(tmp(), "c", "android", "m").unwrap();
        t.add_secret("a", "{{x}}");
        t.add_secret("", "{{y}}");
        assert_eq!(t.redact("a b c"), "a b c");
    }

    #[test]
    fn 截图落盘并返回相对路径() {
        let d = tmp();
        let t = Trace::open(&d, "c", "android", "m").unwrap();
        let rel = t.save_shot(4, "after", b"PNGDATA").unwrap();
        assert_eq!(rel, "shots/04_after.png");
        assert_eq!(std::fs::read(d.join(&rel)).unwrap(), b"PNGDATA");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 落盘没有残留临时文件() {
        let d = tmp();
        let mut t = Trace::open(&d, "c", "android", "m").unwrap();
        t.add_step(step("tap")).unwrap();
        assert!(!d.join("trace.json.tmp").exists(), "原子落盘后临时文件必须已改名");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 坏掉的轨迹文件按新建处理() {
        let d = tmp();
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("trace.json"), "{坏的").unwrap();
        let t = Trace::open(&d, "TC-9", "android", "m").unwrap();
        assert_eq!(t.step_count(), 0, "文件损坏时不能崩，也不能带着脏数据续跑");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn 前一控件文案里的秘密也会脱敏() {
        let dir = tmp();
        let mut t = Trace::open(&dir, "TC-01", "android", "m").unwrap();
        t.add_secret("hunter2pass", "{{account.password}}");
        let mut st = step("tap");
        st.prev_label = Some("密码 hunter2pass".into());
        t.add_step(st).unwrap();
        let text = std::fs::read_to_string(dir.join("trace.json")).unwrap();
        assert!(!text.contains("hunter2pass"), "落盘轨迹不能有明文");
        assert!(text.contains("密码 {{account.password}}"));
    }
}
