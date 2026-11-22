-- P1：项目与用例。字段含义见 docs/数据模型.md。

create table projects (
    id          uuid primary key default gen_random_uuid(),
    name        text        not null unique,
    created_at  timestamptz not null default now()
);

create table test_cases (
    id            uuid primary key default gen_random_uuid(),
    project_id    uuid        not null references projects(id) on delete cascade,
    -- 用例编号，如 TC-04，项目内唯一
    code          text        not null,
    title         text        not null,
    level         text        not null default '',
    platforms     text[]      not null default '{}',
    preconditions jsonb       not null default '[]',
    steps         jsonb       not null,
    -- 数组，每项 {idx, text}，Judge 逐条判定
    expected      jsonb       not null,
    -- 用例数据；账号用占位符 {{account.xxx}}，不存明文
    data          jsonb       not null default '{}',
    refs          jsonb       not null default '[]',
    note          text        not null default '',
    -- 故意写错的负例：正确判定必须是 failed（由 level = '故意失败' 推出，不靠编号后缀）
    should_fail   boolean     not null default false,
    status        text        not null default 'active' check (status in ('active', 'blocked', 'archived')),
    blocked_reason text,
    -- 内容变化时自增；run 记录当时的版本
    version       integer     not null default 1,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    unique (project_id, code),
    check (jsonb_typeof(steps) = 'array' and jsonb_array_length(steps) > 0),
    check (jsonb_typeof(expected) = 'array' and jsonb_array_length(expected) > 0)
);

create index test_cases_project_idx on test_cases (project_id, code);
