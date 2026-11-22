//! 护栏：步数上限、卡住检测、高危拦截。
//!
//! 这些必须放在设备 MCP 服务这一层，不能依赖模型自觉（仓库 CLAUDE.md 规则 4、6）。
//! 规则来自技术验证里已验证过的实现（PoC 的 `mcp_device/server.py`）。本模块是纯逻辑，不碰设备，方便测试。

use std::collections::{BTreeSet, VecDeque};

use device::Element;

/// 高危关键词的内置默认值：支付、删除账号、注销类页面，工具层直接拒绝并返回 `need_human`。
/// 与技术验证的配置一致；可用环境变量 `HAFA_HIGH_RISK`（逗号分隔）整体覆盖。
pub const DEFAULT_HIGH_RISK: [&str; 12] = [
    "支付", "付款", "删除账号", "注销", "充值", "Delete Account", "Deactivate", "Payment", "Pay Now", "Recharge",
    "Top up", "Purchase",
];

/// 记多少个最近的页面签名 / 动作；卡住只看最后 3 个，多留几个方便排查。
const KEEP: usize = 6;
/// 连续多少次页面不变 / 动作重复判卡住。
const STUCK_N: usize = 3;

// ---------------------------------------------------------------- 页面指纹

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// 页面控件树签名：忽略状态栏（时间、网速、电量会变），对元素顺序不敏感。
///
/// 比截图相似度可靠：房间里麦位动画、播放进度会让截图一直在变，
/// 但控件树的结构和文案不变时说明页面其实没动。
pub fn tree_signature(els: &[Element]) -> u64 {
    let mut items: Vec<String> = els
        .iter()
        .filter(|e| !e.in_status_bar())
        .map(|e| {
            let b = e.bounds;
            format!("{}|{},{},{},{}", e.label(), b.left, b.top, b.right, b.bottom)
        })
        .collect();
    items.sort();
    fnv1a(items.join("\n").as_bytes())
}

/// 进度、计时、百分比这类每秒都变的文字，不算「新出现」。
fn is_timer_like(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | '/' | '%' | ' '))
}

fn labels(els: &[Element]) -> BTreeSet<String> {
    els.iter().filter(|e| !e.in_status_bar() && !e.label().is_empty()).map(|e| e.label().trim().to_string()).collect()
}

/// 动作后新出现的文字。Toast 一闪而过，模型再 observe 时往往已经消失，所以在动作返回里直接带回去。
pub fn new_labels(before: &[Element], after: &[Element]) -> Vec<String> {
    let old = labels(before);
    labels(after).into_iter().filter(|l| !old.contains(l) && !is_timer_like(l)).collect()
}

/// 收集触点覆盖的所有元素文案，供高危检查。同一位置的装饰节点可能与按钮重叠。
pub fn labels_at(els: &[Element], x: i32, y: i32) -> String {
    els.iter()
        .filter(|e| e.bounds.contains(x, y))
        .flat_map(|e| [e.desc.as_str(), e.text.as_str()])
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 坐标落在哪个元素上：取「有文字、面积不超过 `max_area`」的最小元素。
/// 坐标点击靠它还原成稳定的名字（脚本生成时用名字定位，不用坐标）。
pub fn element_at(els: &[Element], x: i32, y: i32, max_area: i64) -> Option<&Element> {
    els.iter()
        .filter(|e| !e.label().is_empty() && !e.in_status_bar() && e.bounds.contains(x, y))
        .filter(|e| i64::from(e.bounds.right - e.bounds.left) * i64::from(e.bounds.bottom - e.bounds.top) <= max_area)
        .min_by_key(|e| i64::from(e.bounds.right - e.bounds.left) * i64::from(e.bounds.bottom - e.bounds.top))
}

// ---------------------------------------------------------------- 护栏状态

/// 拒绝继续操作的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// 用例已经结束
    Finished(String),
    /// 超过最大步数，已判阻塞
    MaxSteps(u32),
    /// 触发了高危拦截，必须先 need_human，由人处理后才能继续
    NeedHuman(String),
}

impl Refusal {
    /// 给模型看的话。
    pub fn message(&self) -> String {
        match self {
            Refusal::Finished(r) => format!("用例已结束（{r}），不再接受操作。"),
            Refusal::MaxSteps(n) => format!("已超过最大步数 {n}，判定阻塞。"),
            Refusal::NeedHuman(r) => format!("{r}。请先调用 need_human 说明，等人工处理后再继续。"),
        }
    }
}

#[derive(Debug)]
pub struct Guard {
    max_steps: u32,
    steps: u32,
    high_risk: Vec<String>,
    sigs: VecDeque<Option<u64>>,
    actions: VecDeque<String>,
    finished: Option<String>,
    need_human: Option<String>,
}

impl Guard {
    pub fn new(max_steps: u32, high_risk: Vec<String>) -> Self {
        Self { max_steps, steps: 0, high_risk, sigs: VecDeque::new(), actions: VecDeque::new(), finished: None, need_human: None }
    }

    pub fn steps(&self) -> u32 {
        self.steps
    }

    pub fn max_steps(&self) -> u32 {
        self.max_steps
    }

    pub fn finished(&self) -> Option<&str> {
        self.finished.as_deref()
    }

    pub fn need_human_reason(&self) -> Option<&str> {
        self.need_human.as_deref()
    }

    /// 每个会动设备的工具调用之前先过这一关。
    /// 超过最大步数时直接把用例标成 blocked，之后一切操作都会被拒绝。
    pub fn begin(&mut self) -> Result<(), Refusal> {
        self.begin_read()?;
        if let Some(r) = &self.need_human {
            return Err(Refusal::NeedHuman(r.clone()));
        }
        Ok(())
    }

    /// 只读观察的入口：用例结束 / 超过最大步数仍然要拒绝，但被高危拦截时允许观察——
    /// 模型需要看一眼页面才能向人说明情况；真正动设备的操作用 [`Guard::begin`]。
    pub fn begin_read(&mut self) -> Result<(), Refusal> {
        self.check_unfinished()?;
        if self.steps >= self.max_steps {
            self.finish("blocked".into());
            return Err(Refusal::MaxSteps(self.max_steps));
        }
        Ok(())
    }

    pub(crate) fn check_unfinished(&self) -> Result<(), Refusal> {
        match &self.finished {
            Some(result) => Err(Refusal::Finished(result.clone())),
            None => Ok(()),
        }
    }

    /// 目标文案里是否含高危关键词；大小写不敏感。
    pub fn risk(&self, label: &str) -> Option<&str> {
        let low = label.to_lowercase();
        self.high_risk.iter().find(|k| !k.is_empty() && low.contains(&k.to_lowercase())).map(String::as_str)
    }

    /// 命中高危：记下来，之后所有会动设备的操作都被拒绝，直到人工处理完（`human_resolved`）。
    pub fn block_for_human(&mut self, keyword: &str) -> Refusal {
        let reason = format!("高危动作被拦截：目标包含高危关键词「{keyword}」");
        self.need_human = Some(reason.clone());
        Refusal::NeedHuman(reason)
    }

    /// 模型主动调用 need_human。
    pub fn request_human(&mut self, reason: String) {
        self.need_human = Some(reason);
    }

    /// 人工处理完：清掉拦截状态，并清空页面签名（人工操作会让页面变化，不能让之前的签名触发误判）。
    pub fn human_resolved(&mut self) {
        self.need_human = None;
        self.sigs.clear();
        self.actions.clear();
    }

    /// 一次动作执行完后计一步。页面签名未知时中断页面不变的连续计数。
    pub fn record(&mut self, action: String, page_sig: Option<u64>) {
        self.actions.push_back(action);
        self.sigs.push_back(page_sig);
        while self.actions.len() > KEEP {
            self.actions.pop_front();
        }
        while self.sigs.len() > KEEP {
            self.sigs.pop_front();
        }
        self.steps += 1;
    }

    /// 卡住检测：连续 3 步页面控件树不变，或连续 3 次相同动作。
    pub fn stuck(&self) -> Option<String> {
        if let Some(Some(last)) = self.sigs.back()
            && self.sigs.len() >= STUCK_N
            && self.sigs.iter().rev().take(STUCK_N).all(|sig| *sig == Some(*last))
        {
            return Some("连续 3 步页面控件树没有变化，疑似卡住".into());
        }
        if self.actions.len() >= STUCK_N {
            let last = &self.actions[self.actions.len() - 1];
            if self.actions.iter().rev().take(STUCK_N).all(|a| a == last) {
                return Some(format!("连续 3 次重复动作: {last}"));
            }
        }
        None
    }

    pub fn finish(&mut self, result: String) {
        if self.finished.is_none() {
            self.finished = Some(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use device::Bounds;

    fn el(desc: &str, l: i32, t: i32, r: i32, b: i32) -> Element {
        Element {
            desc: desc.into(), text: String::new(), class: String::new(), resource_id: String::new(),
            bounds: Bounds { left: l, top: t, right: r, bottom: b },
            clickable: true, enabled: true, scrollable: false,
        }
    }

    fn risk_list() -> Vec<String> {
        DEFAULT_HIGH_RISK.iter().map(|s| s.to_string()).collect()
    }

    // ---- 页面指纹
    #[test]
    fn 签名对元素顺序不敏感() {
        let a = [el("A", 0, 100, 10, 200), el("B", 0, 200, 10, 300)];
        let b = [el("B", 0, 200, 10, 300), el("A", 0, 100, 10, 200)];
        assert_eq!(tree_signature(&a), tree_signature(&b));
    }

    #[test]
    fn 签名忽略状态栏() {
        let a = [el("12:00", 0, 0, 100, 60), el("页面", 0, 100, 10, 200)];
        let b = [el("12:01", 0, 0, 100, 60), el("页面", 0, 100, 10, 200)];
        assert_eq!(tree_signature(&a), tree_signature(&b), "状态栏的时间变化不能影响页面签名");
    }

    #[test]
    fn 文案或位置变了签名就变() {
        let base = [el("A", 0, 100, 10, 200)];
        assert_ne!(tree_signature(&base), tree_signature(&[el("B", 0, 100, 10, 200)]));
        assert_ne!(tree_signature(&base), tree_signature(&[el("A", 0, 100, 10, 210)]));
    }

    #[test]
    fn 新出现的文字排除计时() {
        let before = [el("Music(0/50)", 0, 100, 10, 200)];
        let after = [
            el("Music(0/50)", 0, 100, 10, 200),
            el("Music added successfully", 0, 300, 10, 400),
            el("00:12", 0, 400, 10, 500),
            el("45%", 0, 500, 10, 600),
        ];
        assert_eq!(new_labels(&before, &after), ["Music added successfully"], "进度、计时、百分比不算新出现");
    }

    // ---- 卡住检测
    #[test]
    fn 连续三步页面不变判卡住() {
        let mut g = Guard::new(40, vec![]);
        g.record("tap:A".into(), Some(1));
        g.record("tap:B".into(), Some(1));
        assert_eq!(g.stuck(), None, "只有两步不算");
        g.record("tap:C".into(), Some(1));
        assert!(g.stuck().unwrap().contains("页面控件树没有变化"));
    }

    #[test]
    fn 页面中途变化不算卡住() {
        let mut g = Guard::new(40, vec![]);
        for (a, s) in [("a", 1), ("b", 1), ("c", 2), ("d", 2)] {
            g.record(a.into(), Some(s));
        }
        assert_eq!(g.stuck(), None);
    }

    #[test]
    fn 未知页面中断连续不变判定() {
        let mut g = Guard::new(40, vec![]);
        for (action, sig) in [("a", Some(1)), ("b", Some(1)), ("c", None), ("d", Some(1)), ("e", Some(1))] {
            g.record(action.into(), sig);
        }
        assert_eq!(g.stuck(), None);
        g.record("f".into(), Some(1));
        assert!(g.stuck().unwrap().contains("页面控件树没有变化"));
    }

    #[test]
    fn 连续三次相同动作判卡住() {
        let mut g = Guard::new(40, vec![]);
        for s in [1, 2, 3] {
            g.record("tap:Add".into(), Some(s));
        }
        assert!(g.stuck().unwrap().contains("重复动作"), "页面在变，但动作一直重复");
    }

    // ---- 步数上限
    #[test]
    fn 超过最大步数判阻塞且之后全部拒绝() {
        let mut g = Guard::new(2, vec![]);
        g.begin().unwrap();
        g.record("a".into(), Some(1));
        g.begin().unwrap();
        g.record("b".into(), Some(2));
        assert_eq!(g.begin(), Err(Refusal::MaxSteps(2)));
        g.finish("passed".into());
        assert_eq!(g.finished(), Some("blocked"));
        assert!(matches!(g.begin(), Err(Refusal::Finished(_))), "阻塞之后任何操作都要被拒绝");
    }

    #[test]
    fn 被高危拦截时仍可只读观察但不能动设备() {
        let mut g = Guard::new(40, risk_list());
        g.block_for_human("支付");
        assert_eq!(g.begin_read(), Ok(()), "拦截后模型要能看一眼页面才能向人说明");
        assert!(g.begin().is_err(), "但不能再动设备");
        g.finish("failed".into());
        assert!(g.begin_read().is_err(), "用例结束后连观察也拒绝");
    }

    #[test]
    fn 结束后不再接受操作() {
        let mut g = Guard::new(40, vec![]);
        g.finish("passed".into());
        assert_eq!(g.begin(), Err(Refusal::Finished("passed".into())));
    }

    // ---- 高危拦截
    #[test]
    fn 高危关键词大小写不敏感() {
        let g = Guard::new(40, risk_list());
        assert_eq!(g.risk("Pay Now"), Some("Pay Now"));
        assert_eq!(g.risk("pay now"), Some("Pay Now"));
        assert_eq!(g.risk("确认支付 ¥6"), Some("支付"));
        assert_eq!(g.risk("Add Music"), None);
        assert_eq!(g.risk("Delete account"), Some("Delete Account"));
    }

    #[test]
    fn 命中高危后一切操作被拒绝直到人工处理() {
        let mut g = Guard::new(40, risk_list());
        let r = g.block_for_human("支付");
        assert!(r.message().contains("need_human"));
        assert!(matches!(g.begin(), Err(Refusal::NeedHuman(_))), "拦截后模型不能绕过，继续点别的");
        g.human_resolved();
        assert_eq!(g.begin(), Ok(()));
        assert_eq!(g.stuck(), None, "人工处理后签名要清空，避免误判卡住");
    }

    #[test]
    fn 坐标点击不能绕过高危拦截() {
        // 支付按钮的文字与多个小节点重叠，坐标反查仍应命中。
        let mut pay = el("确认", 100, 1300, 620, 1400);
        pay.text = "Pay Now".into();
        let els = [pay, el("", 355, 1345, 365, 1355), el("", 350, 1340, 370, 1360), el("", 345, 1335, 375, 1365)];
        let label = labels_at(&els, 360, 1350);
        assert!(label.contains("Pay Now"), "坐标反查要拿到支付按钮的文案：{label}");
        let g = Guard::new(40, risk_list());
        assert!(g.risk(&label).is_some());
    }

    #[test]
    fn 坐标还原成元素_取有文字的最小元素且面积受限() {
        let els = [el("整页", 0, 100, 720, 1600), el("Add(1)", 22, 1487, 698, 1577)];
        let hit = element_at(&els, 360, 1530, 720 * 1600 * 4 / 10).unwrap();
        assert_eq!(hit.desc, "Add(1)");
        assert!(element_at(&els, 360, 800, 720 * 1600 * 4 / 10).is_none(), "整页容器超过半屏面积，不能当成点击目标");
    }
}
