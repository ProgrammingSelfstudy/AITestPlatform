//! 看门狗：判断 `codex exec` 是不是卡住了。纯逻辑（时间由调用方传入），方便测试。
//!
//! 技术验证里实测出的三种卡住：
//! - `Stalled`：模型太久没有任何进展；
//! - `ToolHang`：工具调用发出后太久没返回（设备服务挂住）；`need_human` 除外，它本来就会等人；
//! - `ReconnectLoop`：自上次进展以来连续太多条 `Reconnecting...`，连接已经坏了，不用再等。

use std::time::{Duration, Instant};

use super::events::Stall;

/// 动过手机之后的续跑次数；还没动手机时多给 1 次干净重试。
pub const MAX_RESUMES: u32 = 2;

/// 看门狗的各项时限。默认值是技术验证里实测定下来的；测试里调小，不用真等几分钟。
#[derive(Debug, Clone)]
pub struct Limits {
    /// 模型多久没有进展算卡住
    pub stall: Duration,
    /// 发出任务后，模型首次响应（第一次工具调用）最多等这么久
    pub start_stall: Duration,
    /// 工具调用发出后这么久还没返回，视为设备服务挂住（`need_human` 除外）
    pub tool_hang: Duration,
    /// 自上次进展以来连续这么多条 Reconnecting 提示，判定连接已坏
    pub reconnect_limit: u32,
    /// 续跑退避的单位：第 n 次等 min(n, 3) 个单位
    pub backoff_unit: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            stall: Duration::from_secs(120),
            start_stall: Duration::from_secs(75),
            tool_hang: Duration::from_secs(90),
            reconnect_limit: 6,
            backoff_unit: Duration::from_secs(5),
        }
    }
}

impl Limits {
    /// 续跑前的退避：网络抖动时立刻重连通常还是失败。第 n 次等 min(n, 3) 个单位（默认 5、10、15、15…秒）。
    pub fn backoff(&self, resume_n: u32) -> Duration {
        self.backoff_unit * resume_n.min(3)
    }
}

#[derive(Debug, Clone)]
pub struct Watch {
    /// 正在执行的工具调用：(工具名, 开始时间)
    pub inflight: Option<(String, Instant)>,
    /// 最近一次「真进展」：工具调用、模型回复、一轮结束。重连提示不算
    pub progress_at: Instant,
    /// 自上次进展以来的 Reconnecting 提示条数
    pub reconnects: u32,
}

impl Watch {
    pub fn new(now: Instant) -> Self {
        Self { inflight: None, progress_at: now, reconnects: 0 }
    }

    /// 收到一条事件后更新状态。`kind` 是事件类型，`item_type` / `tool` 来自 `item`，`message` 来自错误事件。
    pub fn on_event(&mut self, now: Instant, kind: &str, item_type: &str, tool: &str, message: &str) {
        if item_type == "mcp_tool_call" {
            self.inflight = if kind == "item.started" { Some((tool.to_string(), now)) } else { None };
        }
        let is_noise = kind == "error" || item_type == "error";
        if kind == "error" && message.starts_with("Reconnecting") {
            self.reconnects += 1;
        } else if !is_noise {
            self.progress_at = now;
            self.reconnects = 0;
        }
    }

    /// 下一次读 stdout 最多等多久。到点还没有输出就是卡住（见 [`Watch::stall_kind`]）。
    /// `first` 表示还没有任何工具调用（模型首次响应前，等得更短）。
    pub fn wait_limit(&self, now: Instant, deadline: Instant, lim: &Limits, first: bool) -> Duration {
        let until_deadline = deadline.saturating_duration_since(now);
        let quiet = if first { lim.stall.min(lim.start_stall) } else { lim.stall };
        let limit = match &self.inflight {
            // 等人工，不算卡住
            Some((tool, _)) if tool == "need_human" => until_deadline,
            Some((_, since)) => lim.tool_hang.saturating_sub(now.saturating_duration_since(*since)).min(until_deadline),
            None if self.reconnects >= lim.reconnect_limit => Duration::ZERO, // 连接已坏，不用再等
            None => quiet.saturating_sub(now.saturating_duration_since(self.progress_at)).min(until_deadline),
        };
        limit.max(Duration::from_millis(10))
    }

    /// 读 stdout 超时之后，判断是哪种卡住。
    pub fn stall_kind(&self, lim: &Limits) -> Stall {
        if self.inflight.is_some() {
            Stall::ToolHang
        } else if self.reconnects >= lim.reconnect_limit {
            Stall::ReconnectLoop
        } else {
            Stall::Stalled
        }
    }
}

/// 这一次卡住之后还能不能续跑：动过手机之后最多 `MAX_RESUMES` 次；还没动手机时多给 1 次干净重试；
/// 没拿到会话 id 就无法续跑。
pub fn may_resume(resumes: u32, tool_calls: u32, has_thread_id: bool) -> bool {
    has_thread_id && resumes < MAX_RESUMES + u32::from(tool_calls == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn lim(stall: u64) -> Limits {
        Limits { stall: secs(stall), ..Default::default() }
    }

    #[test]
    fn 首次响应前等得更短() {
        let t0 = Instant::now();
        let w = Watch::new(t0);
        let far = t0 + secs(3600);
        assert_eq!(w.wait_limit(t0, far, &lim(120), true), secs(75), "还没有工具调用：75 秒");
        assert_eq!(w.wait_limit(t0, far, &lim(120), false), secs(120));
        assert_eq!(w.wait_limit(t0, far, &lim(30), true), secs(30), "配置得更短就用更短的");
    }

    #[test]
    fn 已经安静了一段时间只剩余量() {
        let t0 = Instant::now();
        let w = Watch::new(t0);
        assert_eq!(w.wait_limit(t0 + secs(100), t0 + secs(3600), &lim(120), false), secs(20));
    }

    #[test]
    fn 整体截止时间优先() {
        let t0 = Instant::now();
        let w = Watch::new(t0);
        assert_eq!(w.wait_limit(t0, t0 + secs(5), &lim(120), false), secs(5));
    }

    #[test]
    fn 工具调用中按工具挂住时限算() {
        let t0 = Instant::now();
        let mut w = Watch::new(t0);
        w.on_event(t0, "item.started", "mcp_tool_call", "tap", "");
        assert_eq!(w.wait_limit(t0 + secs(30), t0 + secs(3600), &lim(120), false), secs(60), "90 - 30");
        assert_eq!(w.stall_kind(&lim(120)), Stall::ToolHang);
    }

    #[test]
    fn need_human期间不算卡住() {
        let t0 = Instant::now();
        let mut w = Watch::new(t0);
        w.on_event(t0, "item.started", "mcp_tool_call", "need_human", "");
        assert_eq!(w.wait_limit(t0 + secs(600), t0 + secs(1800), &lim(120), false), secs(1200), "等人工只受整体截止时间约束");
    }

    #[test]
    fn 工具返回后恢复正常计时() {
        let t0 = Instant::now();
        let mut w = Watch::new(t0);
        w.on_event(t0, "item.started", "mcp_tool_call", "tap", "");
        w.on_event(t0 + secs(5), "item.completed", "mcp_tool_call", "tap", "");
        assert!(w.inflight.is_none());
        assert_eq!(w.progress_at, t0 + secs(5));
        assert_eq!(w.stall_kind(&lim(120)), Stall::Stalled);
    }

    #[test]
    fn 重连提示不算进展且累计到上限立即判连接已坏() {
        let t0 = Instant::now();
        let l = lim(120);
        let mut w = Watch::new(t0);
        for i in 0..l.reconnect_limit {
            w.on_event(t0 + secs(1 + u64::from(i)), "error", "", "", "Reconnecting... 1/5");
        }
        assert_eq!(w.progress_at, t0, "重连提示不能刷新进展时间");
        assert_eq!(w.stall_kind(&l), Stall::ReconnectLoop);
        assert_eq!(w.wait_limit(t0 + secs(7), t0 + secs(3600), &l, false), Duration::from_millis(10), "不用再等");
    }

    #[test]
    fn 真进展会清零重连计数() {
        let t0 = Instant::now();
        let mut w = Watch::new(t0);
        for _ in 0..5 {
            w.on_event(t0, "error", "", "", "Reconnecting... ");
        }
        w.on_event(t0 + secs(2), "item.completed", "agent_message", "", "");
        assert_eq!(w.reconnects, 0);
        assert_eq!(w.progress_at, t0 + secs(2));
    }

    #[test]
    fn 普通错误既不是进展也不是重连() {
        let t0 = Instant::now();
        let mut w = Watch::new(t0);
        w.on_event(t0 + secs(3), "error", "", "", "连接被重置");
        assert_eq!((w.progress_at, w.reconnects), (t0, 0));
    }

    #[test]
    fn 续跑次数限制() {
        assert!(may_resume(0, 5, true) && may_resume(1, 5, true));
        assert!(!may_resume(2, 5, true), "动过手机之后最多续跑 2 次");
        assert!(may_resume(2, 0, true), "还没动手机时多给 1 次干净重试");
        assert!(!may_resume(3, 0, true));
        assert!(!may_resume(0, 5, false), "没有会话 id 无法续跑");
    }

    #[test]
    fn 退避递增并封顶() {
        let l = Limits::default();
        assert_eq!([1, 2, 3, 4, 10].map(|n| l.backoff(n).as_secs()), [5, 10, 15, 15, 15]);
    }

    #[test]
    fn 时间倒流不会下溢() {
        let t0 = Instant::now();
        let w = Watch::new(t0 + secs(10));
        // now 早于 progress_at：饱和减法，不能 panic
        let _ = w.wait_limit(t0, t0 + secs(100), &lim(120), false);
    }
}
