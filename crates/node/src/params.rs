//! 服务端「参数配置」下发给本节点的运行参数（设备命令重试次数、卡住检测阈值）。
//!
//! 节点上线时和配置变化时服务端会发 `SetParams`；没收到之前（或服务端是旧版本）用默认值。
//! 设备命令重试在本进程里直接生效，并通过环境变量传给每次启动的设备 MCP 服务（它是独立进程）。

use std::sync::Mutex;

use protocol::SetParams;

static CURRENT: Mutex<SetParams> = Mutex::new(SetParams { device_retry: 3, stuck_n: 3, stuck_keep: 6 });

/// 当前参数。
pub fn current() -> SetParams {
    *CURRENT.lock().unwrap()
}

/// 应用服务端发来的参数：先收进合法范围（重试不超过 3 次），再让本进程的设备层生效。
pub fn apply(params: SetParams) -> SetParams {
    let clean = SetParams::clamped(params.device_retry, params.stuck_n, params.stuck_keep);
    *CURRENT.lock().unwrap() = clean;
    device::set_max_attempts(clean.device_retry);
    clean
}

/// 启动设备 MCP 服务时要带上的环境变量。
pub fn mcp_env() -> Vec<(String, String)> {
    let p = current();
    vec![
        ("HAFA_DEVICE_RETRY".into(), p.device_retry.to_string()),
        ("HAFA_STUCK_N".into(), p.stuck_n.to_string()),
        ("HAFA_STUCK_KEEP".into(), p.stuck_keep.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 应用参数会收进范围_并生成给设备服务的环境变量() {
        let applied = apply(SetParams { device_retry: 99, stuck_n: 4, stuck_keep: 1 });
        assert_eq!(applied, SetParams { device_retry: 3, stuck_n: 4, stuck_keep: 4 });
        assert_eq!(device::max_attempts(), 3);
        let env = mcp_env();
        assert!(env.contains(&("HAFA_STUCK_N".into(), "4".into())));
        assert!(env.contains(&("HAFA_STUCK_KEEP".into(), "4".into())));
        apply(SetParams::default());
    }
}
