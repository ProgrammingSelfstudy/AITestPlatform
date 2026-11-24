-- 多个判定员同时评审同一份证据：每个判定员各自的结果单独存，不合并。
-- tasks.extra_judges 是主判定员（tasks.judge）之外的其他判定员；judge_opinions.seq 0 是主判定员，1 起是第几个其他判定员。
alter table tasks add column extra_judges jsonb not null default '[]'::jsonb check (jsonb_typeof(extra_judges) = 'array');

create table judge_opinions (
    run_id        uuid        not null references runs(id) on delete cascade,
    seq           integer     not null check (seq >= 0),
    source        text        not null,
    model         text,
    effort        text,
    judge_status  text        not null check (judge_status in ('completed', 'failed', 'not_run')),
    final_result  text        not null check (final_result in ('passed', 'failed', 'uncertain')),
    -- 这个判定员逐条的判定（和 judge_items 同样的内容：idx、verdict、evidence、file_refs），已由服务端核对
    items         jsonb       not null default '[]'::jsonb,
    created_at    timestamptz not null default now(),
    primary key (run_id, seq)
);
