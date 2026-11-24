//! 节点上的 run 执行：执行器 trait、上报器、任务注册表。
//!
//! 一个 run 由一个 [`RunExecutor`] 负责。执行器通过 [`Reporter`] 往服务端上报（开始、步骤、执行结果、判定、受阻）。
//! 规则：
//! - 上报是**往通道里放**，不直接写连接：断线期间产生的消息由节点保留，重连后补发（服务端按 `(run_id, idx)` 幂等）；
//! - 执行器返回了却没给出最终结果（判定完成或受阻），节点自己补一条「受阻」，**不让 run 悬在半空**；
//! - 执行器崩溃（panic）同样补一条受阻；
//! - 取消要真的停掉执行器，包括它派生的内部任务。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use protocol::{BlockedReason, ExecutorDone, FinalResult, JudgeDone, JudgeItem, JudgeStatus, LogLine, ModelSource, NodeMsg, QuotaExhausted, RunAssign, RunBlocked, Step, Verdict};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// 执行器：负责一个 run 从开始到给出最终结果。
pub trait RunExecutor: Send + Sync + 'static {
    fn execute(&self, assign: RunAssign, rep: Reporter) -> impl Future<Output = ()> + Send;
}

/// 没有执行能力的节点：领到任务就如实报告受阻，不让任务悬着。
pub struct NoExecutor;

impl RunExecutor for NoExecutor {
    async fn execute(&self, _assign: RunAssign, rep: Reporter) {
        rep.blocked(BlockedReason::Other, "这个节点还没有执行能力（没有配置执行器）".into());
    }
}

/// 一个 run 的上报器。可以克隆。
#[derive(Clone)]
pub struct Reporter {
    run_id: Uuid,
    node_id: Uuid,
    tx: mpsc::UnboundedSender<NodeMsg>,
    start_confirmed: bool,
    /// 是否已经给出最终结果（判定完成或受阻）
    terminal: Arc<AtomicBool>,
}

impl Reporter {
    pub fn run_id(&self) -> Uuid {
        self.run_id
    }

    pub fn node_id(&self) -> Uuid {
        self.node_id
    }

    fn send(&self, m: NodeMsg) {
        let _ = self.tx.send(m);
    }

    pub fn started(&self) {
        if !self.start_confirmed {
            self.send(NodeMsg::RunStarted { run_id: self.run_id });
        }
    }

    pub fn step(&self, idx: u32, at_ms: i64, tool: &str, args: Value, result: Value, file_ref: Option<String>) {
        self.send(NodeMsg::Step(Step { run_id: self.run_id, idx, at_ms, tool: tool.into(), args, result, file_ref }));
    }

    pub fn executor_done(&self, result: Verdict, reason: String, cost: Option<Value>) {
        self.send(NodeMsg::ExecutorDone(ExecutorDone { run_id: self.run_id, result, reason, cost }));
    }

    /// 判定完成：这是最终结果之一。Judge 没跑成时 `status` 不是 `Completed`，最终结果必须是 `Uncertain`。
    pub fn judge_done(&self, status: JudgeStatus, final_result: FinalResult, items: Vec<JudgeItem>) {
        self.terminal.store(true, Ordering::SeqCst);
        self.send(NodeMsg::JudgeDone(JudgeDone { run_id: self.run_id, judge_status: status, final_result, items }));
    }

    /// 多个判定员时，每个人各自的评审结果；在 `judge_done` 之前上报，服务端按「一致才采用」汇总。
    pub fn judge_opinions(&self, opinions: Vec<protocol::JudgeOpinion>) {
        self.send(NodeMsg::JudgeOpinions(protocol::JudgeOpinions { run_id: self.run_id, opinions }));
    }

    /// 受阻：这是最终结果之一。
    pub fn blocked(&self, reason: BlockedReason, detail: String) {
        self.terminal.store(true, Ordering::SeqCst);
        self.send(NodeMsg::RunBlocked(RunBlocked { run_id: self.run_id, reason, detail }));
    }

    /// 额度用完。尚未结束的 run 判受阻；判定已上报时发送节点级额度通知。
    pub fn quota_exhausted(&self, source: ModelSource) {
        let was_terminal = self.terminal.swap(true, Ordering::SeqCst);
        self.send(NodeMsg::QuotaExhausted(QuotaExhausted { source, run_id: (!was_terminal).then_some(self.run_id) }));
    }

    pub fn log(&self, level: &str, msg: String) {
        self.send(NodeMsg::Log(LogLine { level: level.into(), msg }));
    }

    pub(crate) fn recording_warn(&self, msg: String) {
        tracing::warn!(run_id = %self.run_id, "{msg}");
        self.log("warn", msg);
    }
}

/// 丢弃时中止任务：外层任务被取消时，里面派生的任务也要一起停。
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 节点上所有正在执行的 run。
pub struct Runner<E> {
    exec: Arc<E>,
    out: mpsc::UnboundedSender<NodeMsg>,
    tasks: Mutex<HashMap<Uuid, JoinHandle<()>>>,
}

impl<E: RunExecutor> Runner<E> {
    pub fn new(exec: E, out: mpsc::UnboundedSender<NodeMsg>) -> Self {
        Self { exec: Arc::new(exec), out, tasks: Mutex::new(HashMap::new()) }
    }

    /// 还在执行的 run（顺手清掉已经结束的）。重连时用它告诉服务端「我这里还在跑哪些」。
    pub fn running(&self) -> Vec<Uuid> {
        let mut t = self.tasks.lock().unwrap();
        t.retain(|_, h| !h.is_finished());
        t.keys().copied().collect()
    }

    /// 开始执行一个 run。同一个 run 已经在跑时忽略（服务端重发不能起第二份）。
    pub fn spawn(&self, node_id: Uuid, assign: RunAssign) {
        self.spawn_with_receipt(node_id, assign, false);
    }

    pub(crate) fn spawn_confirmed(&self, node_id: Uuid, assign: RunAssign) {
        self.spawn_with_receipt(node_id, assign, true);
    }

    fn spawn_with_receipt(&self, node_id: Uuid, assign: RunAssign, start_confirmed: bool) {
        let run_id = assign.run_id;
        if self.running().contains(&run_id) {
            tracing::warn!(%run_id, "这个 run 已经在执行，忽略重复下发");
            return;
        }
        let rep = Reporter { run_id, node_id, tx: self.out.clone(), start_confirmed, terminal: Arc::new(AtomicBool::new(false)) };
        let (exec, rep2) = (self.exec.clone(), rep.clone());
        let handle = tokio::spawn(async move {
            let inner = AbortOnDrop(tokio::spawn(async move { exec.execute(assign, rep).await }));
            let mut inner = inner;
            match (&mut inner.0).await {
                Ok(()) if !rep2.terminal.load(Ordering::SeqCst) => rep2.blocked(BlockedReason::Other, "执行器没有给出最终结果就退出了".into()),
                Ok(()) => {}
                Err(e) if e.is_panic() => rep2.blocked(BlockedReason::Other, "执行器崩溃（panic）".into()),
                Err(_) => {} // 被取消：服务端已经知道，不再上报
            }
        });
        self.tasks.lock().unwrap().insert(run_id, handle);
    }

    /// 停掉一个 run 的执行器。返回它是否真的在跑。
    pub fn abort(&self, run_id: Uuid) -> bool {
        match self.tasks.lock().unwrap().remove(&run_id) {
            Some(h) => {
                let was_running = !h.is_finished();
                h.abort(); // 外层任务被取消 → AbortOnDrop 把里面的执行器任务也停掉
                was_running
            }
            None => false,
        }
    }
}

impl<E> Drop for Runner<E> {
    fn drop(&mut self) {
        if let Ok(tasks) = self.tasks.get_mut() {
            for handle in tasks.values() {
                handle.abort();
            }
        }
    }
}
