-- P0：执行节点与设备。
-- 用户、项目、用例、任务等表随对应功能进入后续迁移（见 docs/落地记录.md）。

create table nodes (
    id          uuid primary key,
    caps        jsonb       not null,
    online      boolean     not null default false,
    last_seen   timestamptz not null default now(),
    created_at  timestamptz not null default now()
);

create table devices (
    id           uuid primary key default gen_random_uuid(),
    node_id      uuid        not null references nodes(id) on delete cascade,
    serial       text        not null,
    platform     text        not null,
    model        text,
    os_version   text,
    screen_w     integer,
    screen_h     integer,
    -- 底部坐标偏移（带导航栏的机型底部内容上移），见 PoC 的 bottom_shift
    bottom_shift integer     not null default 0,
    status       text        not null check (status in ('idle', 'busy', 'offline')),
    last_seen    timestamptz not null default now(),
    unique (node_id, serial)
);

create index devices_node_id_idx on devices (node_id);
