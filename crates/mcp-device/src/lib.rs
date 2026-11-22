//! 设备操作 MCP 服务：执行员（`claude` / `codex`）操作真机的唯一入口。
//!
//! 护栏全部做在这一层，不依赖模型自觉（仓库 CLAUDE.md 规则 4、6）：
//! - 步数上限：超过判阻塞，之后一切操作被拒绝；
//! - 卡住检测：连续 3 步页面不变 / 连续 3 次相同动作，在返回里提醒；
//! - 高危拦截：触摸前检查当前控件树，命中支付、删除账号、注销类目标时返回结构化的 `need_human` 并锁住设备动作；
//! - 轨迹：每一步落盘，账号密码一律脱敏成占位符。
//!
//! 定位优先级：content-desc > 坐标。Hafa 是 Flutter，控件树里 text 为空，可读信息全在 content-desc。
//! 文字输入（`input_text`）还没做：`adb input text` 只支持 ASCII，需要单独解决输入法问题。

pub mod dev;
pub mod evidence;
pub mod guard;
pub mod observe;
pub mod trace;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use device::{DeviceError, Element};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{schemars, tool, tool_router, ErrorData};
use serde::Deserialize;
use tokio::sync::Mutex;

use dev::DeviceApi;
use guard::{Guard, Refusal};
use trace::{Trace, TraceStep, now_ms};

/// 服务配置。节点通过环境变量传进来（见 `from_env`）。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub run_dir: PathBuf,
    pub case_id: String,
    pub platform: String,
    pub model: String,
    pub max_steps: u32,
    pub high_risk: Vec<String>,
    /// 被测应用的包名 / 启动 Activity；没配置时 `launch_app` / `restart_app` 明确报错
    pub package: Option<String>,
    pub activity: Option<String>,
    /// `need_human` 最多等多久
    pub human_timeout: Duration,
    /// 动作后等页面稳定的时间
    pub settle: Duration,
    /// 需要脱敏的 (值, 占位符)
    pub secrets: Vec<(String, String)>,
}

impl ServerConfig {
    /// 从环境变量读取。`HAFA_RUN_DIR` 必须给出：没有地方落轨迹就不能执行。
    pub fn from_env() -> Result<Self, String> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let run_dir = get("HAFA_RUN_DIR").ok_or("请设置环境变量 HAFA_RUN_DIR（本次执行的轨迹目录）")?;
        let num = |k: &str, d: u64| get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        let high_risk = match get("HAFA_HIGH_RISK") {
            Some(v) => v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            None => guard::DEFAULT_HIGH_RISK.iter().map(|s| s.to_string()).collect(),
        };
        let secrets = load_secrets(get("HAFA_SECRETS_FILE").as_deref())?;
        Ok(Self {
            run_dir: run_dir.into(),
            case_id: get("HAFA_CASE_ID").unwrap_or_else(|| "TC-00".into()),
            platform: get("HAFA_PLATFORM").unwrap_or_else(|| "android".into()),
            model: get("HAFA_MODEL").unwrap_or_default(),
            max_steps: num("HAFA_MAX_STEPS", 40) as u32,
            high_risk,
            package: get("HAFA_APP_PACKAGE"),
            activity: get("HAFA_APP_ACTIVITY"),
            human_timeout: Duration::from_secs(num("HAFA_HUMAN_TIMEOUT", 1800)),
            settle: Duration::from_millis(600),
            secrets,
        })
    }
}

/// 读取节点写的脱敏清单：JSON 数组 `[[值, 占位符], ...]`。
/// 明文不走命令行或环境变量（会出现在进程列表和启动日志里），读完立即删除文件。
/// 给了路径却读不出来就报错：宁可不执行，也不能在没有脱敏的情况下继续。
fn load_secrets(path: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let Some(path) = path else { return Ok(vec![]) };
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取脱敏清单失败：{e}"));
    let _ = std::fs::remove_file(path);
    serde_json::from_str(&text?).map_err(|_| "脱敏清单格式错误".to_string())
}

struct State {
    guard: Guard,
    trace: Trace,
    /// 最近一次 observe 的元素（去掉状态栏），`tap` 的编号对应它
    last_elements: Vec<Element>,
    screen: Option<(i32, i32)>,
}

struct Inner {
    dev: Arc<dyn DeviceApi>,
    cfg: ServerConfig,
    st: Mutex<State>,
}

#[derive(Clone)]
pub struct DeviceServer(Arc<Inner>);

impl DeviceServer {
    pub fn new(dev: Arc<dyn DeviceApi>, cfg: ServerConfig) -> std::io::Result<Self> {
        let mut trace = Trace::open(&cfg.run_dir, &cfg.case_id, &cfg.platform, &cfg.model)?;
        for (v, p) in &cfg.secrets {
            trace.add_secret(v, p);
        }
        let mut guard = Guard::new(cfg.max_steps, cfg.high_risk.clone());
        // 续跑：接着已有轨迹的步数计数，但不带旧的页面签名（避免误判卡住）
        for _ in 0..trace.counted_steps() {
            guard.record(String::new(), None);
        }
        guard.human_resolved();
        // 从轨迹恢复终态，避免续跑覆盖已结束用例的结果。
        #[derive(Deserialize)]
        struct SavedResult {
            result: Option<String>,
        }
        let saved: SavedResult = serde_json::from_slice(&std::fs::read(trace.dir().join("trace.json"))?)?;
        if let Some(result) = saved.result {
            guard.finish(result);
        }
        Ok(Self(Arc::new(Inner { dev, cfg, st: Mutex::new(State { guard, trace, last_elements: vec![], screen: None }) })))
    }
}

// ---------------------------------------------------------------- 工具参数

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct TapArgs {
    /// observe 返回的元素编号
    pub element_id: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct TapXyArgs {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SwipeArgs {
    /// up / down / left / right：手指滑动的方向（up = 页面内容向上滚）
    pub direction: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DragArgs {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    /// 拖动时长，毫秒，默认 800（慢速拖动，滑块才跟得上）；限制在 200～5000
    pub duration_ms: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct WaitArgs {
    /// 等待秒数（1～30）
    pub seconds: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NeedHumanArgs {
    /// 需要人工做什么
    pub reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct FinishArgs {
    /// passed / failed / uncertain / blocked
    pub result: String,
    pub reason: String,
}

// ---------------------------------------------------------------- 辅助

type ToolResult = Result<CallToolResult, ErrorData>;

fn text(s: impl Into<String>) -> ToolResult {
    Ok(CallToolResult::success(vec![ContentBlock::text(s)]))
}

fn io_err(e: std::io::Error) -> ErrorData {
    ErrorData::internal_error(format!("写轨迹失败：{e}"), None)
}

impl State {
    /// 设备命令失败时保留原始诊断，并替换其中的凭证。
    fn device_err(&self, e: DeviceError) -> ErrorData {
        ErrorData::internal_error(self.trace.redact(&format!("设备命令失败：{e}")), None)
    }

    fn block_risk(&mut self, labels: &str) -> Option<Refusal> {
        let keyword = self.trace.redact(self.guard.risk(labels)?);
        Some(self.guard.block_for_human(&keyword))
    }

    fn refuse(&mut self, r: Refusal) -> ToolResult {
        let message = r.message();
        match r {
            Refusal::NeedHuman(reason) => Ok(CallToolResult::structured_error(serde_json::json!({
                "status": "need_human",
                "reason": reason,
                "message": message,
            }))),
            Refusal::MaxSteps(_) => {
                self.trace.finish("blocked", &message, vec![]).map_err(io_err)?;
                text(message)
            }
            Refusal::Finished(_) => text(message),
        }
    }
}

fn is_timer_like(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | '/' | '%' | ' '))
}

/// 动作后页面上的文字（从上到下、从左到右），去掉计时类文字，最多 30 条。
fn after_labels(els: &[Element]) -> Vec<String> {
    let mut sorted: Vec<&Element> = els.iter().filter(|e| !e.in_status_bar()).collect();
    sorted.sort_by_key(|e| (e.bounds.top, e.bounds.left));
    let mut seen: Vec<String> = vec![];
    for e in sorted {
        let raw = e.label().trim().to_string();
        if !raw.is_empty() && !is_timer_like(&raw) && !seen.contains(&raw) {
            seen.push(raw);
        }
    }
    seen.truncate(30);
    seen
}

fn note_new(new: &[String]) -> String {
    if new.is_empty() {
        return String::new();
    }
    let items: Vec<String> = new.iter().take(6).map(|x| format!("「{}」", x.chars().take(80).collect::<String>())).collect();
    format!("。动作后新出现的文字：{}", items.join("；"))
}

/// 动作之后的统一收尾结果。
struct After {
    unchanged: bool,
    degraded: bool,
    new: Vec<String>,
    stuck: Option<String>,
}

impl Inner {
    async fn screen(&self, st: &mut State) -> Result<(i32, i32), ErrorData> {
        if let Some(s) = st.screen {
            return Ok(s);
        }
        let s = self.dev.screen_size().await.map_err(|e| st.device_err(e))?;
        st.screen = Some(s);
        Ok(s)
    }

    async fn page_elements(&self) -> device::Result<Vec<Element>> {
        let mut els = self.dev.dump().await?;
        els.retain(|e| !e.in_status_bar());
        Ok(els)
    }

    /// 触摸动作必须在持有状态锁时取得当前控件树，才能检查实际触点。
    async fn tree_before_touch(&self, st: &State) -> Result<Vec<Element>, ErrorData> {
        match self.page_elements().await {
            Ok(els) if !els.is_empty() => Ok(els),
            Ok(_) => Err(ErrorData::internal_error("当前控件树没有页面节点。请先 observe。", None)),
            Err(e) => {
                let mut err = st.device_err(e);
                err.message.to_mut().push_str("。请先 observe。");
                Err(err)
            }
        }
    }

    /// 一次动作执行完后的统一收尾：等页面稳定 → 重新取控件树和截图 → 记护栏 → 写轨迹。
    async fn after_action(&self, st: &mut State, before: &[Element], action_sig: String, mut step: TraceStep, started: Instant) -> Result<After, ErrorData> {
        tokio::time::sleep(self.cfg.settle).await;
        let after = match self.page_elements().await {
            Ok(els) if !els.is_empty() => Some(els),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("动作后读取控件树失败：{}", st.trace.redact(&e.to_string()));
                None
            }
        };
        let sig = after.as_deref().map(guard::tree_signature);
        let unchanged = sig.is_some_and(|sig| !before.is_empty() && sig == guard::tree_signature(before));
        let degraded = sig.is_none();
        let after = after.as_deref().unwrap_or_default();
        let new = guard::new_labels(before, after);

        st.guard.record(action_sig, sig);
        let stuck = st.guard.stuck().map(|s| st.trace.redact(&s));

        let idx = st.trace.step_count() + 1;
        if let Ok(png) = self.dev.screenshot().await {
            step.shot = st.trace.save_shot(idx, "after", &png).ok();
        }
        step.degraded = degraded;
        step.after_labels = after_labels(after);
        step.new_labels = new.clone();
        step.duration_ms = started.elapsed().as_millis() as u64;
        st.trace.add_step(step).map_err(io_err)?;
        let new = new.into_iter().map(|label| st.trace.redact(&label)).collect();
        Ok(After { unchanged, degraded, new, stuck })
    }

    fn compose(&self, base: String, a: &After, warn_unchanged: bool) -> String {
        let mut msg = base + &note_new(&a.new);
        if warn_unchanged && a.unchanged {
            msg.push_str("。注意：动作后页面元素没有任何变化，可能点偏（宽节点的中心不一定可点），请 observe 后对照截图改用 tap_xy");
        }
        if a.degraded {
            msg.push_str("。动作后控件树暂不可用，请 observe 确认页面");
        }
        if let Some(s) = &a.stuck {
            msg.push_str(&format!("。注意：{s}"));
        }
        msg
    }

    /// 点击已经检查过的坐标，并按动作前的控件树比较页面变化。
    async fn do_tap(&self, st: &mut State, before: &[Element], (x, y): (i32, i32), tag: String, mut step: TraceStep) -> Result<After, ErrorData> {
        step.at_ms = now_ms();
        step.point = Some((x, y));
        let started = Instant::now();
        self.dev.tap(x, y).await.map_err(|e| st.device_err(e))?;
        self.after_action(st, before, tag, step, started).await
    }
}

#[tool_router(server_handler)]
impl DeviceServer {
    #[tool(description = "观察当前页面：返回带编号标注的截图和元素列表。每次操作后都应调用。只读，不计步数。")]
    pub async fn observe(&self) -> ToolResult {
        let me = &self.0;
        let mut st = me.st.lock().await;
        if let Err(r) = st.guard.begin_read() {
            return st.refuse(r);
        }
        let at_ms = now_ms();
        let started = Instant::now();
        st.last_elements.clear();
        let raw = me.dev.screenshot().await.map_err(|e| st.device_err(e))?;
        let (els, diag) = match me.page_elements().await {
            Ok(els) => (els, None),
            Err(DeviceError::EmptyDump { diag, .. }) => (vec![], Some(diag)),
            Err(err) => return Err(st.device_err(err)),
        };
        let screen_w = if diag.is_none() { me.screen(&mut st).await.map(|s| s.0).unwrap_or(0) } else { 0 };
        let annotated = observe::annotate(&raw, &els).map_err(|e| ErrorData::internal_error(format!("截图标注失败：{e}"), None))?;

        let next = st.trace.step_count() + 1;
        let head = if let Some(diag) = diag {
            let shot = st.trace.save_shot(next, "observe", &raw).map_err(io_err)?;
            let head = format!(
                "步骤 {}/{}。截图: {shot}\n控件树暂时拿不到（已重试 3 次），本次只有截图；可先 wait 几秒再 observe，或对照截图用 tap_xy。控件树仍为空时 tap_xy 会拒绝。\n诊断：{}",
                st.guard.steps(), st.guard.max_steps(), st.trace.redact(&diag),
            );
            st.trace.add_step(TraceStep {
                action: "observe".into(),
                at_ms,
                degraded: true,
                value: Some(diag),
                shot: Some(shot),
                duration_ms: started.elapsed().as_millis() as u64,
                ..Default::default()
            }).map_err(io_err)?;
            head
        } else {
            let listing = observe::render_listing(&els, screen_w);
            let shot = st.trace.save_shot(next, "observe", &observe::to_png(&annotated)).map_err(io_err)?;
            let head = format!("步骤 {}/{}。元素数 {}。截图: {shot}\n{}", st.guard.steps(), st.guard.max_steps(), els.len(), st.trace.redact(&listing));
            st.last_elements = els;
            head
        };

        let jpeg = observe::to_small_jpeg(&annotated, 540, 70);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(head),
            ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(jpeg), "image/jpeg"),
        ]))
    }

    #[tool(description = "点击 observe 元素列表里编号为 element_id 的元素（点击元素中心）。")]
    pub async fn tap(&self, Parameters(a): Parameters<TapArgs>) -> ToolResult {
        let me = &self.0;
        let mut st = me.st.lock().await;
        if let Err(r) = st.guard.begin() {
            return st.refuse(r);
        }
        let n = st.last_elements.len();
        let Some(el) = (a.element_id as usize).checked_sub(1).and_then(|i| st.last_elements.get(i)).cloned() else {
            return text(format!("element_id 越界：{}，当前只有 {n} 个元素，请先 observe。", a.element_id));
        };
        let (cx, cy) = el.bounds.center();
        if (cx, cy) == (0, 0) {
            return text(format!("元素 {} 没有有效坐标，请改用 tap_xy。", a.element_id));
        }
        let label = observe::label(&el);
        let before = me.tree_before_touch(&st).await?;
        if let Some(r) = st.block_risk(&guard::labels_at(&before, cx, cy)).or_else(|| st.block_risk(&label)) {
            return st.refuse(r);
        }
        let Some(index) = before.iter().position(|current| current == &el) else {
            return text(format!("编号 {} 对应的控件已变化，请先 observe。", a.element_id));
        };
        let prev_label = before[..index].iter().rev().map(|e| e.label().to_string()).find(|l| !l.is_empty());
        let target = if el.label().is_empty() { None } else { Some(el.label().to_string()) };
        let step = TraceStep { action: "tap".into(), target, prev_label, ..Default::default() };
        let after = me.do_tap(&mut st, &before, (cx, cy), format!("tap:{label}"), step).await?;
        let label: String = st.trace.redact(&label).chars().take(120).collect();
        text(me.compose(format!("已点击 [{}] {label}", a.element_id), &after, true))
    }

    #[tool(description = "点击屏幕坐标。列表里没有目标、或元素标有「⚠宽节点」时，对照截图用它。")]
    pub async fn tap_xy(&self, Parameters(a): Parameters<TapXyArgs>) -> ToolResult {
        let me = &self.0;
        let mut st = me.st.lock().await;
        if let Err(r) = st.guard.begin() {
            return st.refuse(r);
        }
        let (w, h) = me.screen(&mut st).await.unwrap_or((720, 1600));
        let before = me.tree_before_touch(&st).await?;
        if let Some(r) = st.block_risk(&guard::labels_at(&before, a.x, a.y)) {
            return st.refuse(r);
        }
        let target = guard::element_at(&before, a.x, a.y, i64::from(w) * i64::from(h) * 4 / 10).map(|e| e.label().to_string());
        let step = TraceStep { action: "tap_xy".into(), target, ..Default::default() };
        let after = me.do_tap(&mut st, &before, (a.x, a.y), format!("tap_xy:{},{}", a.x, a.y), step).await?;
        text(me.compose(format!("已点击坐标 ({}, {})", a.x, a.y), &after, true))
    }

    #[tool(description = "滑动。direction 取 up / down / left / right，是手指滑动的方向。")]
    pub async fn swipe(&self, Parameters(a): Parameters<SwipeArgs>) -> ToolResult {
        let me = &self.0;
        let mut st = me.st.lock().await;
        if let Err(r) = st.guard.begin() {
            return st.refuse(r);
        }
        let (w, h) = me.screen(&mut st).await?;
        let (cx, cy) = (w / 2, h / 2);
        let (x1, y1, x2, y2) = match a.direction.to_lowercase().as_str() {
            "up" => (cx, h * 65 / 100, cx, h * 35 / 100),
            "down" => (cx, h * 35 / 100, cx, h * 65 / 100),
            "left" => (w * 80 / 100, cy, w * 20 / 100, cy),
            "right" => (w * 20 / 100, cy, w * 80 / 100, cy),
            other => return text(format!("direction 只能是 up / down / left / right，收到 {other:?}")),
        };
        let before = st.last_elements.clone();
        let at_ms = now_ms();
        let started = Instant::now();
        me.dev.swipe(x1, y1, x2, y2, 300).await.map_err(|e| st.device_err(e))?;
        let step = TraceStep { action: "swipe".into(), at_ms, value: Some(a.direction.clone()), ..Default::default() };
        let after = me.after_action(&mut st, &before, format!("swipe:{}", a.direction), step, started).await?;
        text(me.compose(format!("已滑动 {}", a.direction), &after, false))
    }

    #[tool(description = "从 (x1,y1) 按住拖到 (x2,y2)。滑块、拖拽用它；swipe 只能滑动整个页面。拖到屏幕左右边缘会触发系统的侧滑返回，起点终点的 x 要离边缘至少 40px。")]
    pub async fn drag(&self, Parameters(a): Parameters<DragArgs>) -> ToolResult {
        let me = &self.0;
        let mut st = me.st.lock().await;
        if let Err(r) = st.guard.begin() {
            return st.refuse(r);
        }
        let before = me.tree_before_touch(&st).await?;
        if let Some(r) = st.block_risk(&guard::labels_at(&before, a.x1, a.y1)) {
            return st.refuse(r);
        }
        let ms = a.duration_ms.unwrap_or(800).clamp(200, 5000);
        let target = guard::element_at(&before, a.x1, a.y1, i64::MAX).map(|e| e.label().to_string());
        let at_ms = now_ms();
        let started = Instant::now();
        me.dev.swipe(a.x1, a.y1, a.x2, a.y2, ms).await.map_err(|e| st.device_err(e))?;
        let step = TraceStep {
            action: "drag".into(),
            at_ms,
            target,
            value: Some(format!("({},{})->({},{}) {ms}ms", a.x1, a.y1, a.x2, a.y2)),
            point: Some((a.x1, a.y1)),
            ..Default::default()
        };
        let after = me.after_action(&mut st, &before, format!("drag:{},{}->{},{}", a.x1, a.y1, a.x2, a.y2), step, started).await?;
        text(me.compose(format!("已拖动 ({}, {}) → ({}, {})", a.x1, a.y1, a.x2, a.y2), &after, true))
    }

    #[tool(description = "按返回键。")]
    pub async fn back(&self) -> ToolResult {
        let me = &self.0;
        let mut st = me.st.lock().await;
        if let Err(r) = st.guard.begin() {
            return st.refuse(r);
        }
        let before = st.last_elements.clone();
        let at_ms = now_ms();
        let started = Instant::now();
        me.dev.back().await.map_err(|e| st.device_err(e))?;
        let after = me.after_action(&mut st, &before, "back".into(), TraceStep { action: "back".into(), at_ms, ..Default::default() }, started).await?;
        text(me.compose("已返回".into(), &after, false))
    }

    #[tool(description = "启动被测应用（不清数据）。")]
    pub async fn launch_app(&self) -> ToolResult {
        self.app_action(false).await
    }

    #[tool(description = "重启被测应用：先强制停止再启动。页面卡死或需要回到首页时用。")]
    pub async fn restart_app(&self) -> ToolResult {
        self.app_action(true).await
    }

    #[tool(description = "等待指定秒数（1～30），用于等待加载、进度变化。")]
    pub async fn wait(&self, Parameters(a): Parameters<WaitArgs>) -> ToolResult {
        let me = &self.0;
        let secs = a.seconds.clamp(1, 30);
        {
            let mut st = me.st.lock().await;
            if let Err(r) = st.guard.begin() {
                return st.refuse(r);
            }
        }
        let at_ms = now_ms();
        let started = Instant::now();
        tokio::time::sleep(Duration::from_secs(secs)).await; // 不持锁等待，status 等调用不被挡住
        let mut st = me.st.lock().await;
        // 等待期间其他工具可能用完步数、触发人工介入或结束用例。
        if let Err(r) = st.guard.begin() {
            return st.refuse(r);
        }
        let before = st.last_elements.clone();
        let step = TraceStep { action: "wait".into(), at_ms, value: Some(secs.to_string()), ..Default::default() };
        let after = me.after_action(&mut st, &before, format!("wait:{secs}"), step, started).await?;
        text(me.compose(format!("已等待 {secs} 秒"), &after, false))
    }

    #[tool(description = "无法继续时调用：说明需要人工做什么。会暂停并等待人工处理完成，处理完后再 observe 确认页面。")]
    pub async fn need_human(&self, Parameters(a): Parameters<NeedHumanArgs>) -> ToolResult {
        let me = &self.0;
        let (req, done) = {
            let mut st = me.st.lock().await;
            if let Err(r) = st.guard.check_unfinished() {
                return st.refuse(r);
            }
            let at_ms = now_ms();
            let reason = st.trace.redact(&a.reason);
            st.guard.request_human(reason.clone());
            let _ = st.trace.add_step(TraceStep { action: "need_human".into(), at_ms, value: Some(reason.clone()), ..Default::default() });
            let dir = st.trace.dir().to_path_buf();
            let (req, done) = (dir.join("need_human.json"), dir.join("human_done.json"));
            let _ = std::fs::remove_file(&done);
            std::fs::write(&req, serde_json::json!({"reason": reason, "ts_ms": at_ms}).to_string()).map_err(io_err)?;
            (req, done)
        };
        // 真正暂停：请求写给节点，节点把人工处理结果写成 human_done.json；这里不持锁轮询
        let deadline = Instant::now() + me.cfg.human_timeout;
        while Instant::now() < deadline {
            if let Ok(t) = std::fs::read_to_string(&done) {
                let note = serde_json::from_str::<serde_json::Value>(&t).ok().and_then(|v| v["note"].as_str().map(str::to_string)).unwrap_or_default();
                let _ = std::fs::remove_file(&req);
                let mut st = me.st.lock().await;
                if let Err(r) = st.guard.check_unfinished() {
                    return st.refuse(r);
                }
                st.guard.human_resolved();
                return text(format!("人工已处理完成（{}）。请先 observe 确认当前页面，再继续后面的步骤。", st.trace.redact(&note)));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let _ = std::fs::remove_file(&req);
        text(format!("等待人工超过 {} 秒，没有收到处理结果。请调用 finish(blocked) 说明。", me.cfg.human_timeout.as_secs()))
    }

    #[tool(description = "结束用例。result 取 passed / failed / uncertain / blocked。会自动留存终态截图和页面文字，供独立的 Judge 复核。")]
    pub async fn finish(&self, Parameters(a): Parameters<FinishArgs>) -> ToolResult {
        let me = &self.0;
        let result = a.result.to_lowercase().trim().to_string();
        if !["passed", "failed", "uncertain", "blocked"].contains(&result.as_str()) {
            return text("result 只能是 passed / failed / uncertain / blocked。");
        }
        let mut st = me.st.lock().await;
        if let Err(r) = st.guard.check_unfinished() {
            return st.refuse(r);
        }
        if result == "passed" && let Some(reason) = st.guard.need_human_reason() {
            let refusal = Refusal::NeedHuman(reason.to_owned());
            return st.refuse(refusal);
        }
        let mut evidence = vec![];
        // 取证失败不能影响结束
        match (me.dev.screenshot().await, me.page_elements().await) {
            (Ok(png), Ok(els)) => {
                let idx = st.trace.step_count() + 1;
                if let Ok(rel) = st.trace.save_shot(idx, "final", &png) {
                    evidence.push(rel);
                }
                let page = st.trace.dir().join("final_page.txt");
                if std::fs::write(&page, st.trace.redact(&observe::render_listing(&els, 0))).is_ok() {
                    evidence.push("final_page.txt".into());
                }
            }
            (a, b) => {
                let shot_err = a.err().map(|e| st.trace.redact(&e.to_string()));
                let tree_err = b.err().map(|e| st.trace.redact(&e.to_string()));
                evidence.push(format!("取证失败：{shot_err:?} / {tree_err:?}"));
            }
        }
        st.trace.finish(&result, &a.reason, evidence).map_err(io_err)?;
        st.guard.finish(result.clone());
        text(format!("已结束：{result}。理由：{}", st.trace.redact(&a.reason)))
    }

    #[tool(description = "返回当前步数和是否已结束，不操作设备。")]
    pub async fn status(&self) -> ToolResult {
        let st = self.0.st.lock().await;
        text(
            serde_json::json!({
                "steps": st.guard.steps(),
                "max_steps": st.guard.max_steps(),
                "finished": st.guard.finished(),
                "need_human": st.guard.need_human_reason(),
            })
            .to_string(),
        )
    }
}

impl DeviceServer {
    async fn app_action(&self, stop_first: bool) -> ToolResult {
        let me = &self.0;
        let mut st = me.st.lock().await;
        if let Err(r) = st.guard.begin() {
            return st.refuse(r);
        }
        let (Some(pkg), Some(act)) = (me.cfg.package.clone(), me.cfg.activity.clone()) else {
            return text("没有配置被测应用的包名 / Activity（HAFA_APP_PACKAGE / HAFA_APP_ACTIVITY），无法启动应用。");
        };
        let before = st.last_elements.clone();
        let at_ms = now_ms();
        let started = Instant::now();
        if stop_first {
            me.dev.stop_app(&pkg).await.map_err(|e| st.device_err(e))?;
        }
        me.dev.launch_app(&pkg, &act).await.map_err(|e| st.device_err(e))?;
        let name = if stop_first { "restart_app" } else { "launch_app" };
        // 冷启动要几秒：多等一会儿再取页面
        tokio::time::sleep(me.cfg.settle * 8).await;
        let step = TraceStep { action: name.into(), at_ms, ..Default::default() };
        let after = me.after_action(&mut st, &before, name.into(), step, started).await?;
        text(me.compose(if stop_first { "已重启应用".into() } else { "已启动应用".into() }, &after, false))
    }
}

#[cfg(test)]
mod secrets_tests {
    use super::load_secrets;

    #[test]
    fn 读取脱敏清单后文件立即删除() {
        let path = std::env::temp_dir().join(format!("secrets_test_{}.json", std::process::id()));
        std::fs::write(&path, r#"[["hunter2pass","{{account.password}}"]]"#).unwrap();
        let got = load_secrets(path.to_str()).unwrap();
        assert_eq!(got, vec![("hunter2pass".to_string(), "{{account.password}}".to_string())]);
        assert!(!path.exists(), "读完必须删除，不留明文");
    }

    #[test]
    fn 给了路径却读不出来就报错() {
        assert!(load_secrets(Some("/不存在/secrets.json")).is_err());
        assert_eq!(load_secrets(None).unwrap(), vec![]);
    }
}
