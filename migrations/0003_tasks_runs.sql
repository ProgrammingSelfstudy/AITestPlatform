-- P1：任务、run（一条用例的一次执行）、步骤、逐条判定、证据文件。字段含义见 docs/数据模型.md。
-- 账号租约（account_leases）和人工审核（reviews）随各自的功能进入后续迁移。

create table tasks (
    id          uuid primary key default gen_random_uuid(),
    project_id  uuid        not null references projects(id) on delete cascade,
    -- 任务绑定到一个具体节点：谁发起谁付额度，任务只能在这个节点上、用这个节点的模型来源执行，没有跨节点调度
    node_id     uuid        not null references nodes(id),
    -- 角色模型快照：{"source": "codex_exec" | "claude_cli", "model": 可空}
    executor    jsonb       not null,
    judge       jsonb       not null,
    created_at  timestamptz not null default now()
);

create table runs (
    id               uuid primary key default gen_random_uuid(),
    task_id          uuid        not null references tasks(id) on delete cascade,
    case_id          uuid        not null references test_cases(id),
    -- 创建时的用例版本，以及用例内容的快照（下发、判定都用快照）：排队期间用例被改了，这次执行也不受影响，能复现当时的用例
    case_version     integer     not null,
    case_spec        jsonb       not null,
    node_id          uuid        not null references nodes(id),
    device_id        uuid        references devices(id),
    status           text        not null default 'queued'
                     check (status in ('queued', 'assigned', 'running', 'judging', 'finished', 'blocked', 'cancelled', 'node_lost')),
    executor_result  text        check (executor_result in ('passed', 'failed', 'uncertain')),
    judge_status     text        check (judge_status in ('completed', 'failed', 'not_run')),
    final_result     text        check (final_result in ('passed', 'failed', 'uncertain', 'blocked')),
    -- 统计完成率时要区分链路 / 环境故障
    blocked_reason   text        check (blocked_reason in ('setup', 'link', 'node_lost', 'stuck', 'quota', 'other')),
    executor_reason  text,
    cost             jsonb,
    -- 排队顺序：单调递增。不能靠 queued_at：同一个任务的多个 run 在同一个事务里创建，now() 完全相同，排序会退化成按随机 id
    seq              bigint      generated always as identity,
    queued_at        timestamptz not null default now(),
    assigned_at      timestamptz,
    started_at       timestamptz,
    node_lost_at     timestamptz,
    finished_at      timestamptz,

    -- Judge 没跑成时最终结果只能是「待确认」：不得用执行员的自评顶替
    constraint judge_failed_means_uncertain check (judge_status is distinct from 'failed' or final_result = 'uncertain'),
    -- 阻塞的 run 最终结果就是 blocked，且必须说明原因
    constraint blocked_has_reason check ((status = 'blocked') = (blocked_reason is not null)),
    constraint blocked_final check (status <> 'blocked' or final_result = 'blocked'),
    -- 只有走完判定的 run 才有 passed / failed / uncertain 的最终结果
    constraint finished_has_final check (status <> 'finished' or final_result in ('passed', 'failed', 'uncertain')),
    constraint active_has_device check (status not in ('assigned', 'running', 'judging', 'node_lost') or device_id is not null)
);

-- 设备占用锁：同一台设备同一时刻只能有一个活跃 run。放在库里，不靠应用层判断。
-- node_lost（节点掉线的宽限期内）也占着设备：否则节点重连前，别的排队任务会把这台手机领走，想续跑时设备已被占。
create unique index runs_device_lock on runs (device_id) where status in ('assigned', 'running', 'judging', 'node_lost');
-- 领任务时按节点和排队时间取
create index runs_queue_idx on runs (node_id, seq) where status = 'queued';
create index runs_task_idx on runs (task_id);

create table run_steps (
    run_id    uuid        not null references runs(id) on delete cascade,
    -- (run_id, idx) 保证重传幂等
    idx       integer     not null,
    tool      text        not null,
    -- 已脱敏
    args      jsonb       not null default '{}',
    result    jsonb       not null default '{}',
    file_ref  text,
    ts        timestamptz not null default now(),
    primary key (run_id, idx)
);

create table judge_items (
    run_id         uuid    not null references runs(id) on delete cascade,
    idx            integer not null,
    expected_text  text    not null default '',
    verdict        text    not null check (verdict in ('passed', 'failed', 'uncertain')),
    evidence       text    not null default '',
    file_refs      text[]  not null default '{}',
    primary key (run_id, idx)
);

create table files (
    id          uuid primary key default gen_random_uuid(),
    run_id      uuid        not null references runs(id) on delete cascade,
    kind        text        not null check (kind in ('screenshot', 'recording', 'trace', 'page_text')),
    -- 录屏分段存多条，用 seq 排序
    seq         integer     not null default 0,
    path        text        not null,
    size        bigint      not null,
    sha256      text        not null,
    created_at  timestamptz not null default now()
);
create index files_run_idx on files (run_id, kind, seq);
