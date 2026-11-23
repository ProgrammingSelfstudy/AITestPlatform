-- 模块、项目级需求文档、用例生成请求。
-- 文档是项目级的、各模块共用；按内容摘要去重，同一份内容只存一次。
-- 用例生成请求照回归请求（0015）的做法：排在发起人自己的节点上，由节点领取、生成、回报（规则 9）。

create table modules (
    id          uuid primary key default gen_random_uuid(),
    project_id  uuid        not null references projects(id) on delete cascade,
    name        text        not null check (char_length(name) between 1 and 80),
    created_at  timestamptz not null default now(),
    unique (project_id, name)
);

-- 老用例（导入的）默认已批准、不属于任何模块，行为不变。
-- 生成的用例先是 draft，经评审员复核和人工确认变成 approved 才能执行。
alter table test_cases add column module_id uuid references modules(id) on delete set null;
alter table test_cases add column origin text not null default 'imported' check (origin in ('imported', 'generated'));
alter table test_cases add column review_status text not null default 'approved'
    check (review_status in ('draft', 'reviewed_ok', 'needs_human', 'approved', 'rejected'));

create table project_docs (
    id          uuid primary key default gen_random_uuid(),
    project_id  uuid        not null references projects(id) on delete cascade,
    sha256      text        not null,
    content     text        not null check (char_length(content) <= 200000),
    created_at  timestamptz not null default now(),
    unique (project_id, sha256)
);

create table casegen_requests (
    id            uuid        primary key default gen_random_uuid(),
    project_id    uuid        not null references projects(id) on delete cascade,
    module_id     uuid        not null references modules(id) on delete cascade,
    doc_id        uuid        not null references project_docs(id),
    node_id       uuid        not null references nodes(id),
    requested_by  uuid        not null references users(id),
    -- UI 设计图链接：只做记录和跳转，平台和节点都不会去打开它们
    ui_links      jsonb       not null,
    -- 角色 id（节点本机配置里的角色）；校验在创建时按节点上报的角色做
    writer_role   text,
    reviewer_role text,
    status        text        not null default 'queued' check (status in ('queued', 'running', 'finished', 'failed', 'cancelled')),
    detail        text        not null default '' check (char_length(detail) <= 2000),
    requested_at  timestamptz not null default clock_timestamp(),
    claimed_at    timestamptz,
    finished_at   timestamptz
);

-- 同一个节点上同一个模块同时只能有一个未结束的生成请求：连点两次不会排两份
create unique index casegen_requests_active on casegen_requests (node_id, module_id) where status in ('queued', 'running');
create index casegen_requests_queue_idx on casegen_requests (node_id, requested_at) where status = 'queued';
create index casegen_requests_module_idx on casegen_requests (module_id, requested_at desc);

-- 用户上传的 UI 区域截图（AI 只看这些，不看设计工具里的任何东西）
create table casegen_screenshots (
    id           uuid primary key default gen_random_uuid(),
    request_id   uuid  not null references casegen_requests(id) on delete cascade,
    seq          integer not null,
    content_type text  not null check (content_type in ('image/png', 'image/jpeg')),
    bytes        bytea not null,
    sha256       text  not null,
    unique (request_id, seq)
);
