-- P3：脚本归档。只存「合规候选 → 自检通过 → 获准」的回归脚本版本，不存模型原始输出。
-- 脚本来源必须是 Judge 整体通过的一次执行；内容不含账号明文（只有 {{account.xxx}} 占位符）。

create table scripts (
    id               uuid primary key default gen_random_uuid(),
    case_id          uuid        not null references test_cases(id) on delete cascade,
    -- 同一用例内从 1 递增，由归档模块在锁住用例行后分配
    version          integer     not null check (version >= 1),
    -- 脚本依据的用例版本（test_cases.version）；用例内容变了，旧版本的脚本不再适用
    case_version     integer     not null check (case_version >= 1),
    -- 生成这版脚本所依据的那次执行；执行记录不删，脚本可以一直追溯到来源
    source_run_id    uuid        not null references runs(id),
    -- {相对路径: 源码}，只含 test_<编号>.py 和可选的 pages/<编号>_pages.py
    files            jsonb       not null,
    -- 整套脚本的内容摘要：按路径排序，逐个文件把「路径 NUL 单文件摘要 换行」连起来再取 SHA-256
    content_sha256   text        not null check (content_sha256 ~ '^[0-9a-f]{64}$'),
    -- 生成时用的模型来源、模型名和强度（来自角色配置，不写死）
    gen_source       text        not null check (gen_source in ('claude_cli', 'codex_exec')),
    gen_model        text,
    gen_effort       text,
    notes            text        not null default '' check (char_length(notes) <= 4000),
    -- candidate 刚生成；self_check_passed 在受控环境自检通过；approved 可被平台选来回归；
    -- rejected 被拒绝；retired 获准后下线。迁移规则由归档模块保证，库里只限定取值。
    status           text        not null default 'candidate'
                     check (status in ('candidate', 'self_check_passed', 'approved', 'rejected', 'retired')),
    status_note      text        check (char_length(status_note) <= 2000),
    created_at       timestamptz not null default clock_timestamp(),
    updated_at       timestamptz not null default clock_timestamp(),
    unique (case_id, version),
    -- 同一用例同样的内容只归档一次，重复生成直接指回已有版本
    unique (case_id, content_sha256),
    check (jsonb_typeof(files) = 'object' and files <> '{}'::jsonb)
);

create index scripts_case_idx on scripts (case_id, version desc);
create index scripts_status_idx on scripts (status, case_id);
