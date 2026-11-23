-- 页面发起的回归请求：人在页面上点「回归」，请求排在某个节点上，由节点自己来领取并在自己的手机上跑。
-- 和任务派发分开：回归不花模型额度，只占一台手机；但仍然只能发到发起人自己的节点（规则 9）。
create table regression_requests (
    id           uuid        primary key default gen_random_uuid(),
    script_id    uuid        not null references scripts(id),
    node_id      uuid        not null references nodes(id),
    -- 领取时才确定用哪台设备（节点只带一台设备去领）；排队时为空
    serial       text,
    status       text        not null default 'queued' check (status in ('queued', 'running', 'finished', 'failed', 'cancelled')),
    -- finished：节点跑完并记录了回归结论（结论本身在 script_regressions，可能通过也可能失败）
    -- failed：没能得到结论（环境问题、脚本已不是获准版本、节点没有回报），原因写在这里
    detail       text        not null default '' check (char_length(detail) <= 2000),
    requested_at timestamptz not null default clock_timestamp(),
    claimed_at   timestamptz,
    finished_at  timestamptz,
    constraint running_has_serial check (status <> 'running' or serial is not null)
);

-- 同一个脚本在同一个节点上同时只能有一个未结束的请求：连点两次不会排两份
create unique index regression_requests_active on regression_requests (script_id, node_id) where status in ('queued', 'running');
-- 一台设备同时只能被一个回归占用
create unique index regression_requests_device_lock on regression_requests (node_id, serial) where status = 'running';
create index regression_requests_queue_idx on regression_requests (node_id, requested_at) where status = 'queued';
create index regression_requests_script_idx on regression_requests (script_id, requested_at desc);
