-- 发起人身份：个人令牌（只存 SHA-256 摘要）。节点首次带个人令牌注册时绑定属主。
create table users (
    id          uuid primary key default gen_random_uuid(),
    name        text        not null unique,
    token_hash  text        not null unique,
    created_at  timestamptz not null default now()
);

alter table nodes add column owner_user_id uuid references users(id);
