-- 浏览器会话只存令牌的 SHA-256 摘要。删除用户时一并删除会话。
create table user_sessions (
    token_hash  text primary key,
    user_id     uuid        not null references users(id) on delete cascade,
    expires_at  timestamptz not null,
    created_at  timestamptz not null default now()
);
