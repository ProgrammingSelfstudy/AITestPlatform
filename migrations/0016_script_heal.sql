-- 自愈：获准脚本回归失败后，在节点上做一次分诊。分诊只有三种结论，其中只有「定位失效」会产出新的候选版本。
-- 候选版本记下它是从哪个版本修来的；获准、下线仍是人的决定，不因为是修复版就放宽。
alter table scripts add column healed_from uuid references scripts(id);

create table script_triages (
    id                uuid        primary key default gen_random_uuid(),
    script_id         uuid        not null references scripts(id),
    -- 分诊针对的那次失败回归：同一次失败只分诊一次，避免重复花模型额度，也让「每次失败的结论」可追溯
    regression_id     uuid        not null unique references script_regressions(id),
    node_id           uuid        not null references nodes(id),
    -- locator_broken：控件定位失效，产出了修复候选；business_bug：业务行为不符合预期，保留失败、进入缺陷流程；
    -- unsure：分不清或没能修好，交人工，保留原结果和版本
    verdict           text        not null check (verdict in ('locator_broken', 'business_bug', 'unsure')),
    reason            text        not null check (char_length(reason) between 1 and 1000),
    healed_script_id  uuid        references scripts(id),
    created_at        timestamptz not null default clock_timestamp(),
    -- 只有「定位失效」才有修复候选
    check ((verdict = 'locator_broken') = (healed_script_id is not null))
);

create index script_triages_script_idx on script_triages (script_id, created_at desc);
