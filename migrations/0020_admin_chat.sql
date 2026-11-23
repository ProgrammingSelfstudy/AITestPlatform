-- 管理员聊天：每个人和管理员的对话，以及管理员的提议。
-- 管理员只提议，**人点确认才会执行**：提议先是 pending，人确认后服务端用人的身份去做，没做成记 failed 和原因，人忽略记 dismissed。
create table admin_messages (
    id          uuid        primary key default gen_random_uuid(),
    user_id     uuid        not null references users(id),
    author      text        not null check (author in ('user', 'admin')),
    text        text        not null check (char_length(text) <= 30000),
    -- 管理员没回答成功（没有角色、额度用完、超时…）时为真，text 是原因
    is_error    boolean     not null default false,
    created_at  timestamptz not null default clock_timestamp()
);
create index admin_messages_user_idx on admin_messages (user_id, created_at);

create table admin_proposals (
    id           uuid        primary key default gen_random_uuid(),
    message_id   uuid        not null references admin_messages(id) on delete cascade,
    user_id      uuid        not null references users(id),
    -- 回答时用的节点：确认后发起测试就落在这个节点上（必须还是这个人的）
    node_id      uuid        not null references nodes(id),
    kind         text        not null check (kind in ('create_module', 'generate_cases', 'run_tests')),
    args         jsonb       not null,
    why          text        not null default '' check (char_length(why) <= 600),
    status       text        not null default 'pending' check (status in ('pending', 'confirmed', 'dismissed', 'failed')),
    result       text        not null default '' check (char_length(result) <= 2000),
    created_at   timestamptz not null default clock_timestamp(),
    decided_at   timestamptz
);
create index admin_proposals_message_idx on admin_proposals (message_id);
