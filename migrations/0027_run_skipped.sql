-- 条件不足而跳过的用例：记成一条已结束的 run（status = blocked），原因写 'skipped'，具体说明放在 executor_reason。
-- 它不会被派发、不占设备，也不算通过；页面上显示「已跳过 · 原因」，和真正的「受阻」（链路、环境故障）区分开。
alter table runs drop constraint runs_blocked_reason_check;
alter table runs add constraint runs_blocked_reason_check
    check (blocked_reason in ('setup', 'link', 'node_lost', 'stuck', 'quota', 'other', 'skipped'));
