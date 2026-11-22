-- 绑定到设备序列号的账号只供该设备使用；未绑定的账号沿用原有派发规则。
alter table test_accounts add column device_serial text;
