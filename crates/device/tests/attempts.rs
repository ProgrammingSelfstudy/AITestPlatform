//! 重试次数是进程级设置，单独一个测试文件（单独进程），不会和别的测试抢这个全局值。

#[test]
fn 重试次数能配置_但永远不超过三次() {
    assert_eq!(device::max_attempts(), 3, "默认 3 次");
    device::set_max_attempts(2);
    assert_eq!(device::max_attempts(), 2);
    device::set_max_attempts(99);
    assert_eq!(device::max_attempts(), 3, "写多大都不能超过项目规则的 3 次");
    device::set_max_attempts(0);
    assert_eq!(device::max_attempts(), 1, "至少尝试一次");
}
