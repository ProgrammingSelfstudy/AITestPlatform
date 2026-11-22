//! 设备 MCP 服务的工具行为测试：用假设备，覆盖护栏的每一条规则。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use device::{Bounds, DeviceError, Element};
use mcp_device::dev::DeviceApi;
use mcp_device::{DeviceServer, DragArgs, FinishArgs, NeedHumanArgs, ServerConfig, SwipeArgs, TapArgs, TapXyArgs, WaitArgs};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;

// ---------------------------------------------------------------- 假设备

fn el(desc: &str, clickable: bool, l: i32, t: i32, r: i32, b: i32) -> Element {
    Element {
        desc: desc.into(), text: String::new(), class: "android.view.View".into(), resource_id: String::new(),
        bounds: Bounds { left: l, top: t, right: r, bottom: b }, clickable, enabled: true, scrollable: false,
    }
}

type NextFn = Box<dyn Fn(usize, (i32, i32)) -> usize + Send + Sync>;

struct Fake {
    pages: Vec<Vec<Element>>,
    cur: AtomicUsize,
    next: NextFn,
    taps: Mutex<Vec<(i32, i32)>>,
    swipes: Mutex<Vec<(i32, i32, i32, i32)>>,
    calls: Mutex<Vec<String>>,
    fail_tap: AtomicBool,
    fail_dump_after_tap: AtomicBool,
    empty_dump: Mutex<Option<String>>,
}

impl Fake {
    fn new(pages: Vec<Vec<Element>>, next: NextFn) -> Arc<Self> {
        Arc::new(Self { pages, cur: AtomicUsize::new(0), next, taps: Mutex::new(vec![]), swipes: Mutex::new(vec![]), calls: Mutex::new(vec![]), fail_tap: AtomicBool::new(false), fail_dump_after_tap: AtomicBool::new(false), empty_dump: Mutex::new(None) })
    }
    /// 点击后页面不变的单页设备
    fn still(page: Vec<Element>) -> Arc<Self> {
        Self::new(vec![page], Box::new(|c, _| c))
    }
    fn taps(&self) -> Vec<(i32, i32)> {
        self.taps.lock().unwrap().clone()
    }
    fn set_empty_dump(&self, diag: &str) {
        *self.empty_dump.lock().unwrap() = Some(diag.into());
    }
}

#[async_trait]
impl DeviceApi for Fake {
    async fn dump(&self) -> device::Result<Vec<Element>> {
        if let Some(diag) = self.empty_dump.lock().unwrap().clone() {
            return Err(DeviceError::EmptyDump { attempts: 3, diag });
        }
        Ok(self.pages[self.cur.load(Ordering::SeqCst)].clone())
    }
    async fn screenshot(&self) -> device::Result<Vec<u8>> {
        let img = image::RgbImage::from_pixel(720, 1600, image::Rgb([255, 255, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img).write_to(&mut buf, image::ImageFormat::Png).unwrap();
        Ok(buf.into_inner())
    }
    async fn tap(&self, x: i32, y: i32) -> device::Result<()> {
        if self.fail_tap.load(Ordering::SeqCst) {
            return Err(DeviceError::Command { cmd: "adb shell input tap".into(), code: Some(1), stderr: "device offline".into() });
        }
        self.taps.lock().unwrap().push((x, y));
        let c = self.cur.load(Ordering::SeqCst);
        self.cur.store((self.next)(c, (x, y)), Ordering::SeqCst);
        if self.fail_dump_after_tap.load(Ordering::SeqCst) {
            self.set_empty_dump("mCurrentFocus=null");
        }
        Ok(())
    }
    async fn swipe(&self, x1: i32, y1: i32, x2: i32, y2: i32, _ms: u32) -> device::Result<()> {
        self.swipes.lock().unwrap().push((x1, y1, x2, y2));
        Ok(())
    }
    async fn back(&self) -> device::Result<()> {
        self.calls.lock().unwrap().push("back".into());
        Ok(())
    }
    async fn launch_app(&self, p: &str, a: &str) -> device::Result<()> {
        self.calls.lock().unwrap().push(format!("launch {p}/{a}"));
        Ok(())
    }
    async fn stop_app(&self, p: &str) -> device::Result<()> {
        self.calls.lock().unwrap().push(format!("stop {p}"));
        Ok(())
    }
    async fn screen_size(&self) -> device::Result<(i32, i32)> {
        Ok((720, 1600))
    }
}

// ---------------------------------------------------------------- 辅助

fn run_dir() -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!("mcp_tools_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
}

fn cfg(dir: &std::path::Path, max_steps: u32) -> ServerConfig {
    ServerConfig {
        run_dir: dir.into(), case_id: "TC-T".into(), platform: "android".into(), model: "fake".into(), max_steps,
        high_risk: mcp_device::guard::DEFAULT_HIGH_RISK.iter().map(|s| s.to_string()).collect(),
        package: None, activity: None, human_timeout: Duration::from_secs(5), settle: Duration::ZERO, secrets: vec![],
    }
}

fn server(dev: Arc<Fake>, max_steps: u32) -> (DeviceServer, std::path::PathBuf) {
    let d = run_dir();
    (DeviceServer::new(dev, cfg(&d, max_steps)).unwrap(), d)
}

fn txt(r: &CallToolResult) -> String {
    r.content.iter().filter_map(|c| c.as_text().map(|t| t.text.clone())).collect::<Vec<_>>().join("\n")
}

fn assert_need_human(r: &CallToolResult, keyword: &str) {
    assert_eq!(r.is_error, Some(true));
    let result = r.structured_content.as_ref().expect("应返回结构化的人工介入结果");
    assert_eq!(result["status"], "need_human");
    let reason = result["reason"].as_str().expect("应说明人工介入原因");
    assert!(reason.contains("高危") && reason.contains(keyword), "{reason}");
}

async fn tap(s: &DeviceServer, id: u32) -> String {
    txt(&s.tap(Parameters(TapArgs { element_id: id })).await.unwrap())
}

async fn tap_xy(s: &DeviceServer, x: i32, y: i32) -> String {
    txt(&s.tap_xy(Parameters(TapXyArgs { x, y })).await.unwrap())
}

async fn status(s: &DeviceServer) -> serde_json::Value {
    serde_json::from_str(&txt(&s.status().await.unwrap())).unwrap()
}

fn list_page() -> Vec<Element> {
    vec![el("Title", false, 0, 100, 720, 160), el("Add Music", true, 22, 1487, 698, 1577)]
}

fn pay_page() -> Vec<Element> {
    vec![el("Checkout", false, 0, 100, 720, 160), el("Pay Now", true, 100, 1300, 620, 1400), el("Cancel", true, 100, 1450, 620, 1550)]
}

// ---------------------------------------------------------------- 测试

#[tokio::test]
async fn observe返回编号列表和缩小后的标注截图且不计步数() {
    let (s, d) = server(Fake::still(list_page()), 40);
    let r = s.observe().await.unwrap();
    let t = txt(&r);
    assert!(t.contains("[1] Title (只读)") && t.contains("[2] Add Music (可点)"), "{t}");
    assert!(t.contains("⚠宽节点"), "Add Music 宽度超过屏幕 80% 要提示：{t}");
    let img = r.content.iter().find_map(|c| c.as_image().cloned()).expect("应带截图");
    assert_eq!(img.mime_type, "image/jpeg");
    assert_eq!(status(&s).await["steps"], 0, "observe 不计步数");
    assert!(d.join("shots/01_observe.png").exists(), "标注图要落盘供排查");
}

#[tokio::test]
async fn 空控件树观察降级为原图和脱敏诊断并留轨迹() {
    let fake = Fake::still(list_page());
    fake.set_empty_dump("mCurrentFocus=null tester@example.com s3cret");
    let dir = run_dir();
    let mut config = cfg(&dir, 1);
    config.secrets = vec![
        ("tester@example.com".into(), "{{account.username}}".into()),
        ("s3cret".into(), "{{account.password}}".into()),
    ];
    let server = DeviceServer::new(fake.clone(), config.clone()).unwrap();
    let reply = server.observe().await.expect("空控件树不应返回工具错误");
    assert_ne!(reply.is_error, Some(true));
    assert_eq!(reply.content.len(), 2);
    let msg = txt(&reply);
    assert!(msg.contains("控件树暂时拿不到（已重试 3 次），本次只有截图；可先 wait 几秒再 observe，或对照截图用 tap_xy"), "{msg}");
    assert!(msg.contains("诊断：mCurrentFocus=null {{account.username}} {{account.password}}"), "{msg}");
    assert!(!msg.contains("Add Music") && !msg.contains("tester@example.com") && !msg.contains("s3cret"), "{msg}");
    let img = reply.content.iter().find_map(|part| part.as_image()).expect("应返回截图");
    assert_eq!(img.mime_type, "image/jpeg");
    assert!(!img.data.is_empty());
    assert_eq!(status(&server).await["steps"], 0);
    server.observe().await.unwrap();
    let trace = std::fs::read_to_string(dir.join("trace.json")).unwrap();
    assert!(!trace.contains("tester@example.com") && !trace.contains("s3cret"), "{trace}");
    let trace: serde_json::Value = serde_json::from_str(&trace).unwrap();
    let steps = trace["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_ne!(steps[0]["shot"], steps[1]["shot"], "降级截图不可互相覆盖");
    for step in steps {
        assert_eq!(step["action"], "observe");
        assert_eq!(step["degraded"], true);
        assert!(step["at_ms"].as_u64().unwrap() > 1_000_000_000_000);
        assert!(step["value"].as_str().unwrap().contains("mCurrentFocus=null"));
        assert_eq!(step["after_labels"], serde_json::json!([]));
        assert_eq!(std::fs::read(dir.join(step["shot"].as_str().unwrap())).unwrap(), fake.screenshot().await.unwrap());
    }
    let resumed = DeviceServer::new(fake, config).unwrap();
    assert_eq!(status(&resumed).await["steps"], 0, "续跑也不计观察步数");
}

#[tokio::test]
async fn 等待步骤记录开始时间而不是结束落盘时间() {
    let (server, dir) = server(Fake::still(list_page()), 40);
    server.wait(Parameters(WaitArgs { seconds: 1 })).await.unwrap();
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("trace.json")).unwrap()).unwrap();
    let step = &trace["steps"][0];
    let at_ms = step["at_ms"].as_u64().unwrap();
    let end = step["ts_ms"].as_u64().unwrap();
    assert!(at_ms > 1_000_000_000_000 && end - at_ms >= 900);
}

#[tokio::test]
async fn 从未观察且控件树为空时拒绝坐标点击() {
    let fake = Fake::still(list_page());
    fake.set_empty_dump("mCurrentFocus=null");
    let (server, _) = server(fake.clone(), 40);
    let err = server.tap_xy(Parameters(TapXyArgs { x: 360, y: 1532 })).await.unwrap_err();
    assert!(err.message.contains("连续 3 次 dump 都没有拿到控件节点"), "{err:?}");
    assert!(fake.taps().is_empty());
    assert_eq!(status(&server).await["steps"], 0);
}

#[tokio::test]
async fn 观察降级后不能用旧控件树点击或拖动() {
    let fake = Fake::still(list_page());
    let (server, _) = server(fake.clone(), 40);
    server.observe().await.unwrap();
    fake.set_empty_dump("mCurrentFocus=null");
    server.observe().await.unwrap();
    assert!(tap(&server, 2).await.contains("element_id 越界"));
    let err = server.tap_xy(Parameters(TapXyArgs { x: 360, y: 1532 })).await.unwrap_err();
    assert!(err.message.contains("连续 3 次 dump 都没有拿到控件节点"), "{err:?}");
    let err = server.drag(Parameters(DragArgs { x1: 360, y1: 1532, x2: 360, y2: 500, duration_ms: None })).await.unwrap_err();
    assert!(err.message.contains("连续 3 次 dump 都没有拿到控件节点"), "{err:?}");
    assert!(fake.taps().is_empty() && fake.swipes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn 有观察缓存时动作前取树失败也拒绝并要求观察() {
    let fake = Fake::still(list_page());
    let dir = run_dir();
    let mut config = cfg(&dir, 40);
    config.secrets = vec![("虚构密码123".into(), "{{account.password}}".into())];
    let s = DeviceServer::new(fake.clone(), config).unwrap();
    s.observe().await.unwrap();
    fake.set_empty_dump("mCurrentFocus=null 虚构密码123");
    let replies = [
        s.tap(Parameters(TapArgs { element_id: 2 })).await,
        s.tap_xy(Parameters(TapXyArgs { x: 360, y: 1532 })).await,
        s.drag(Parameters(DragArgs { x1: 360, y1: 1532, x2: 360, y2: 500, duration_ms: None })).await,
    ];
    for reply in replies {
        let err = reply.unwrap_err();
        assert!(err.message.contains("请先 observe") && err.message.contains("mCurrentFocus=null"), "{err:?}");
        assert!(err.message.contains("{{account.password}}") && !err.message.contains("虚构密码123"), "{err:?}");
    }
    assert!(fake.taps().is_empty() && fake.swipes.lock().unwrap().is_empty());
    assert_eq!(status(&s).await["steps"], 0);
}

#[tokio::test]
async fn 按编号点击_点元素中心并写轨迹() {
    let fake = Fake::new(vec![list_page(), vec![el("Local Music Library(22)", false, 0, 100, 720, 160)]], Box::new(|_, _| 1));
    let (s, d) = server(fake.clone(), 40);
    s.observe().await.unwrap();
    let m = tap(&s, 2).await;
    assert!(m.contains("已点击 [2] Add Music"), "{m}");
    assert!(!m.contains("没有任何变化"), "页面变了不应有警告：{m}");
    assert_eq!(fake.taps(), [(360, 1532)], "点的是元素中心");
    assert_eq!(status(&s).await["steps"], 1);

    let trace = std::fs::read_to_string(d.join("trace.json")).unwrap();
    assert!(trace.contains("\"target\": \"Add Music\"") && trace.contains("\"prev_label\": \"Title\""), "{trace}");
    assert!(trace.contains("Local Music Library(22)"), "动作后页面文字要进轨迹，脚本生成靠它：{trace}");
    assert!(d.join("shots/01_after.png").exists());
}

#[tokio::test]
async fn 点击后页面不变要提醒可能点偏() {
    let (s, _d) = server(Fake::still(list_page()), 40);
    s.observe().await.unwrap();
    let m = tap(&s, 2).await;
    assert!(m.contains("页面元素没有任何变化"), "{m}");
}

#[tokio::test]
async fn 动作后新出现的toast直接带回() {
    let toast = vec![el("Title", false, 0, 100, 720, 160), el("Music added successfully", false, 100, 800, 620, 860), el("00:12", false, 0, 900, 100, 950)];
    let (s, _d) = server(Fake::new(vec![list_page(), toast], Box::new(|_, _| 1)), 40);
    s.observe().await.unwrap();
    let m = tap(&s, 2).await;
    assert!(m.contains("动作后新出现的文字：「Music added successfully」"), "{m}");
    assert!(!m.contains("00:12"), "计时类文字不算新出现：{m}");
}

#[tokio::test]
async fn 编号越界不动设备() {
    let fake = Fake::still(list_page());
    let (s, _d) = server(fake.clone(), 40);
    s.observe().await.unwrap();
    assert!(tap(&s, 9).await.contains("element_id 越界"));
    assert!(tap(&s, 0).await.contains("element_id 越界"), "0 也是越界，不能 usize 下溢");
    assert!(fake.taps().is_empty());
    assert_eq!(status(&s).await["steps"], 0);
}

#[tokio::test]
async fn 高危目标被拦截_设备没被点_之后不能绕过继续点别的() {
    let fake = Fake::still(pay_page());
    let (s, _d) = server(fake.clone(), 40);
    s.observe().await.unwrap();
    let reply = s.tap(Parameters(TapArgs { element_id: 2 })).await.unwrap();
    assert_need_human(&reply, "Pay Now");
    assert!(fake.taps().is_empty(), "高危目标绝不能真的点下去");

    // 拦截之后换个安全的按钮也不行：必须先让人处理
    let reply = s.tap(Parameters(TapArgs { element_id: 3 })).await.unwrap();
    assert_need_human(&reply, "Pay Now");
    assert_need_human(&s.back().await.unwrap(), "Pay Now");
    let reply = s.finish(Parameters(FinishArgs { result: "passed".into(), reason: "全部符合".into() })).await.unwrap();
    assert_need_human(&reply, "Pay Now");
    assert!(fake.taps().is_empty());
    assert!(fake.calls.lock().unwrap().is_empty());
    // 但可以只读观察，让模型能向人说明
    s.observe().await.expect("拦截后仍允许观察");
    let state = status(&s).await;
    assert!(state["need_human"].as_str().unwrap().contains("Pay Now"));
    assert!(state["finished"].is_null());
    assert_eq!(state["steps"], 0);
}

#[tokio::test]
async fn 观察后页面变成高危页时点击和拖动均拒绝且不计步() {
    let mut danger = list_page();
    danger[1].desc = "确认注销".into();
    for action in ["tap", "tap_xy", "drag"] {
        let fake = Fake::new(vec![list_page(), danger.clone()], Box::new(|c, _| c));
        let (s, _) = server(fake.clone(), 40);
        s.observe().await.unwrap();
        fake.cur.store(1, Ordering::SeqCst);

        let reply = match action {
            "tap" => s.tap(Parameters(TapArgs { element_id: 2 })).await,
            "tap_xy" => s.tap_xy(Parameters(TapXyArgs { x: 360, y: 1532 })).await,
            "drag" => s.drag(Parameters(DragArgs { x1: 360, y1: 1532, x2: 360, y2: 500, duration_ms: None })).await,
            _ => unreachable!(),
        }.unwrap();
        assert_need_human(&reply, "注销");
        assert!(fake.taps().is_empty() && fake.swipes.lock().unwrap().is_empty(), "{action} 不应发送到设备");
        assert_eq!(status(&s).await["steps"], 0, "拒绝的 {action} 不应计步");
    }
}

#[tokio::test]
async fn 编号目标已变更时拒绝点击原坐标() {
    let mut changed = list_page();
    changed[1].desc = "Settings".into();
    let fake = Fake::new(vec![list_page(), changed], Box::new(|c, _| c));
    let (s, _) = server(fake.clone(), 40);
    s.observe().await.unwrap();
    fake.cur.store(1, Ordering::SeqCst);
    assert!(tap(&s, 2).await.contains("请先 observe"));
    assert!(fake.taps().is_empty());
    assert_eq!(status(&s).await["steps"], 0);
}

#[tokio::test]
async fn 按坐标点击也不能绕过高危拦截() {
    let fake = Fake::still(pay_page());
    let (s, _d) = server(fake.clone(), 40);
    // 故意不先 observe：控件树要现取，否则坐标点击就能绕过
    let m = tap_xy(&s, 360, 1350).await;
    assert!(m.contains("高危"), "{m}");
    assert!(fake.taps().is_empty());
}

#[tokio::test]
async fn 人工处理流程_暂停_写请求_收到完成后恢复() {
    let fake = Fake::still(pay_page());
    let (s, d) = server(fake.clone(), 40);
    s.observe().await.unwrap();
    tap(&s, 2).await; // 触发高危拦截

    let s2 = s.clone();
    let waiting = tokio::spawn(async move { s2.need_human(Parameters(NeedHumanArgs { reason: "请关闭支付页".into() })).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!waiting.is_finished(), "没收到人工处理结果前必须一直暂停");
    let req = std::fs::read_to_string(d.join("need_human.json")).expect("要给节点写出人工请求");
    assert!(req.contains("请关闭支付页"));

    std::fs::write(d.join("human_done.json"), r#"{"note":"已关闭支付页"}"#).unwrap();
    let r = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("收到处理结果后要恢复").unwrap();
    assert!(txt(&r).contains("人工已处理完成（已关闭支付页）"), "{}", txt(&r));
    assert!(!d.join("need_human.json").exists(), "处理完要清掉请求文件");

    let m = tap(&s, 3).await; // 人工处理完之后可以继续
    assert!(m.contains("已点击"), "{m}");
    assert_eq!(fake.taps().len(), 1);
}

#[tokio::test]
async fn 人工等待超时要告诉模型去finish_blocked() {
    let d = run_dir();
    let mut c = cfg(&d, 40);
    c.human_timeout = Duration::from_millis(600);
    let s = DeviceServer::new(Fake::still(list_page()), c).unwrap();
    let r = s.need_human(Parameters(NeedHumanArgs { reason: "x".into() })).await.unwrap();
    assert!(txt(&r).contains("finish(blocked)"), "{}", txt(&r));
}

#[tokio::test]
async fn 超过最大步数判阻塞_之后一切被拒绝() {
    let (s, _d) = server(Fake::still(list_page()), 2);
    s.observe().await.unwrap();
    tap(&s, 2).await;
    tap(&s, 2).await;
    let m = tap(&s, 2).await;
    assert!(m.contains("已超过最大步数 2"), "{m}");
    assert_eq!(status(&s).await["finished"], "blocked");
    assert!(txt(&s.observe().await.unwrap()).contains("用例已结束"), "阻塞之后连观察都拒绝");
}

#[tokio::test]
async fn 等待期间并发动作耗尽步数后等待不再计步() {
    let (s, dir) = server(Fake::still(list_page()), 1);
    let (waiting, action) = tokio::join!(
        biased;
        s.wait(Parameters(WaitArgs { seconds: 1 })),
        s.back(),
    );
    assert!(txt(&action.unwrap()).contains("返回"));
    assert!(txt(&waiting.unwrap()).contains("已超过最大步数 1"));
    let state = status(&s).await;
    assert_eq!(state["steps"], 1);
    assert_eq!(state["finished"], "blocked");
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("trace.json")).unwrap()).unwrap();
    assert_eq!(trace["steps"].as_array().unwrap().len(), 1);
    assert_eq!(trace["steps"][0]["action"], "back");
    assert_eq!(trace["result"], "blocked");
}

#[tokio::test]
async fn 步数受阻后结束工具不能改判通过_续跑也一样() {
    let (s, dir) = server(Fake::still(list_page()), 1);
    s.back().await.unwrap();
    assert!(txt(&s.back().await.unwrap()).contains("已超过最大步数 1"));
    let path = dir.join("trace.json");
    let before = std::fs::read(&path).unwrap();
    let resumed = DeviceServer::new(Fake::still(list_page()), cfg(&dir, 1)).unwrap();
    for server in [s, resumed] {
        let reply = server.finish(Parameters(FinishArgs { result: "passed".into(), reason: "全部符合".into() })).await.unwrap();
        assert!(txt(&reply).contains("用例已结束（blocked）"));
        assert_eq!(status(&server).await["finished"], "blocked");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let trace: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(trace["result"], "blocked");
    assert!(trace["reason"].as_str().unwrap().contains("最大步数"));
}

#[tokio::test]
async fn 连续三次不同坐标页面不变判卡住() {
    let (s, _d) = server(Fake::still(list_page()), 40);
    s.observe().await.unwrap();
    tap_xy(&s, 10, 10).await;
    tap_xy(&s, 20, 20).await;
    let m = tap_xy(&s, 30, 30).await;
    assert!(m.contains("疑似卡住"), "{m}");
}

#[tokio::test]
async fn 连续三次动作后取树失败仍计步并降级且不误报卡住() {
    let fake = Fake::still(list_page());
    fake.fail_dump_after_tap.store(true, Ordering::SeqCst);
    let (s, dir) = server(fake.clone(), 40);
    s.observe().await.unwrap();
    for x in [10, 20, 30] {
        *fake.empty_dump.lock().unwrap() = None;
        let reply = tap_xy(&s, x, 120).await;
        assert!(reply.contains("已点击坐标"), "{reply}");
        assert!(!reply.contains("没有任何变化") && !reply.contains("疑似卡住"), "{reply}");
    }
    assert_eq!(fake.taps(), [(10, 120), (20, 120), (30, 120)]);
    assert_eq!(status(&s).await["steps"], 3);
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("trace.json")).unwrap()).unwrap();
    let steps = trace["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3);
    for step in steps {
        assert_eq!(step["action"], "tap_xy");
        assert_eq!(step["degraded"], true);
        assert_eq!(step["after_labels"], serde_json::json!([]));
        assert_eq!(step["new_labels"], serde_json::json!([]));
        assert!(dir.join(step["shot"].as_str().unwrap()).exists());
    }
}

#[tokio::test]
async fn 设备命令失败时原始错误带出且不记为一步() {
    let fake = Fake::still(list_page());
    let (s, d) = server(fake.clone(), 40);
    s.observe().await.unwrap();
    fake.fail_tap.store(true, Ordering::SeqCst);
    let res = s.tap(Parameters(TapArgs { element_id: 2 })).await;
    let err = match res {
        Err(e) => e.to_string(),
        Ok(r) => panic!("设备失败时应返回错误，实际返回了 Ok：{r:?}"),
    };
    assert!(err.contains("设备命令失败") && err.contains("device offline"), "原始错误要原样带出：{err}");
    assert_eq!(status(&s).await["steps"], 0, "失败的动作不能算一步");
    let trace = std::fs::read_to_string(d.join("trace.json")).unwrap();
    assert!(!trace.contains("\"action\": \"tap\""), "失败的动作不能写进轨迹");
}

#[tokio::test]
async fn finish留存终态证据_之后拒绝操作() {
    let (s, d) = server(Fake::still(list_page()), 40);
    assert!(txt(&s.finish(Parameters(FinishArgs { result: "maybe".into(), reason: "x".into() })).await.unwrap()).contains("result 只能是"));
    let m = txt(&s.finish(Parameters(FinishArgs { result: " Passed ".into(), reason: "全部符合".into() })).await.unwrap());
    assert!(m.contains("已结束：passed"), "{m}");
    assert!(d.join("final_page.txt").exists());
    assert!(d.join("shots/01_final.png").exists(), "终态截图要留存，给独立 Judge 用");
    let trace = std::fs::read_to_string(d.join("trace.json")).unwrap();
    assert!(trace.contains("\"result\": \"passed\"") && trace.contains("全部符合"));
    assert!(tap(&s, 1).await.contains("用例已结束（passed）"));
}

#[tokio::test]
async fn 账号密码不进轨迹_列表_终态页面() {
    let d = run_dir();
    let mut c = cfg(&d, 40);
    c.secrets = vec![("hunter2pass".into(), "{{account.password}}".into())];
    let page = vec![el("Password: hunter2pass", true, 22, 300, 698, 400)];
    let s = DeviceServer::new(Fake::still(page), c).unwrap();
    let obs = txt(&s.observe().await.unwrap());
    assert!(!obs.contains("hunter2pass") && obs.contains("{{account.password}}"), "observe 给模型的列表里也要脱敏：{obs}");
    tap(&s, 1).await;
    s.finish(Parameters(FinishArgs { result: "passed".into(), reason: "输入了 hunter2pass".into() })).await.unwrap();
    for f in ["trace.json", "final_page.txt"] {
        let t = std::fs::read_to_string(d.join(f)).unwrap();
        assert!(!t.contains("hunter2pass"), "{f} 里泄露了密码：{t}");
    }
}

#[tokio::test]
async fn 没配置包名时启动应用明确报错_配置后按先停后启() {
    let fake = Fake::still(list_page());
    let (s, _d) = server(fake.clone(), 40);
    assert!(txt(&s.launch_app().await.unwrap()).contains("没有配置被测应用"));
    assert!(fake.calls.lock().unwrap().is_empty());

    let d = run_dir();
    let mut c = cfg(&d, 40);
    c.package = Some("com.hafa.pikka".into());
    c.activity = Some("com.hafa.pikka.MainActivity".into());
    let fake = Fake::still(list_page());
    let s = DeviceServer::new(fake.clone(), c).unwrap();
    s.restart_app().await.unwrap();
    assert_eq!(*fake.calls.lock().unwrap(), ["stop com.hafa.pikka", "launch com.hafa.pikka/com.hafa.pikka.MainActivity"], "重启必须先强停再启动");
}

#[tokio::test]
async fn 滑动方向和坐标() {
    let fake = Fake::still(list_page());
    let (s, _d) = server(fake.clone(), 40);
    s.swipe(Parameters(SwipeArgs { direction: "UP".into() })).await.unwrap();
    assert_eq!(fake.swipes.lock().unwrap()[0], (360, 1040, 360, 560), "up = 手指向上滑，页面内容向上滚");
    let m = txt(&s.swipe(Parameters(SwipeArgs { direction: "diagonal".into() })).await.unwrap());
    assert!(m.contains("direction 只能是"), "{m}");
    assert_eq!(fake.swipes.lock().unwrap().len(), 1, "非法方向不能动设备");
}

#[tokio::test]
async fn 续跑接着已有步数() {
    let d = run_dir();
    {
        let s = DeviceServer::new(Fake::still(list_page()), cfg(&d, 40)).unwrap();
        s.observe().await.unwrap();
        tap(&s, 2).await;
        tap(&s, 2).await;
    }
    let s = DeviceServer::new(Fake::still(list_page()), cfg(&d, 40)).unwrap();
    assert_eq!(status(&s).await["steps"], 2, "重新启动后要接着已有轨迹的步数");
    s.observe().await.unwrap();
    tap(&s, 2).await;
    assert_eq!(status(&s).await["steps"], 3);
}

#[tokio::test]
async fn 拖动按住拖到终点并写轨迹() {
    let fake = Fake::still(vec![el("Room volume", false, 44, 1400, 676, 1460)]);
    let (s, d) = server(fake.clone(), 40);
    let m = txt(&s.drag(Parameters(DragArgs { x1: 48, y1: 1431, x2: 300, y2: 1431, duration_ms: None })).await.unwrap());
    assert!(m.contains("已拖动 (48, 1431) → (300, 1431)"), "{m}");
    assert_eq!(fake.swipes.lock().unwrap()[0], (48, 1431, 300, 1431));
    assert_eq!(status(&s).await["steps"], 1);
    let trace = std::fs::read_to_string(d.join("trace.json")).unwrap();
    assert!(trace.contains("\"action\": \"drag\"") && trace.contains("Room volume") && trace.contains("800ms"), "默认 800ms，目标名要记下来：{trace}");
}

#[tokio::test]
async fn 拖动时长被限制在合理范围() {
    let fake = Fake::still(list_page());
    let (s, d) = server(fake.clone(), 40);
    s.drag(Parameters(DragArgs { x1: 100, y1: 700, x2: 200, y2: 700, duration_ms: Some(1) })).await.unwrap();
    s.drag(Parameters(DragArgs { x1: 100, y1: 700, x2: 200, y2: 700, duration_ms: Some(999_999) })).await.unwrap();
    let trace = std::fs::read_to_string(d.join("trace.json")).unwrap();
    assert!(trace.contains("200ms") && trace.contains("5000ms"), "过短 / 过长都要夹住：{trace}");
}

#[tokio::test]
async fn 拖动起点在高危元素上也被拦截() {
    let fake = Fake::still(pay_page());
    let (s, _d) = server(fake.clone(), 40);
    let m = txt(&s.drag(Parameters(DragArgs { x1: 360, y1: 1350, x2: 360, y2: 700, duration_ms: None })).await.unwrap());
    assert!(m.contains("高危"), "{m}");
    assert!(fake.swipes.lock().unwrap().is_empty(), "高危起点绝不能真的拖下去");
    let m2 = txt(&s.drag(Parameters(DragArgs { x1: 10, y1: 10, x2: 20, y2: 20, duration_ms: None })).await.unwrap());
    assert!(m2.contains("need_human"), "拦截后换个起点也不行：{m2}");
}
