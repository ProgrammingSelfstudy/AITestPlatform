-- 回归记录：某个已获准的脚本版本在某个节点的某台设备上跑过一次的结果。
-- 只记脚本本身跑出的结论（通过或失败）；设备掉线、前置状态失败这类环境问题不是脚本的结论，不记。
-- 失败的详情是 pytest 的原始输出末尾，原样给人看。
create table script_regressions (
    id          uuid        primary key default gen_random_uuid(),
    script_id   uuid        not null references scripts(id),
    node_id     uuid        not null references nodes(id),
    serial      text        not null,
    passed      boolean     not null,
    detail      text        not null default '' check (char_length(detail) <= 2000),
    duration_ms integer     not null check (duration_ms >= 0),
    created_at  timestamptz not null default clock_timestamp()
);

create index script_regressions_script_idx on script_regressions (script_id, created_at desc);
