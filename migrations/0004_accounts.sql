-- P1：账号池与租约。凭证列只保存 AES-GCM 密文，用户名哈希只用于去重。
create table test_accounts (
    id             uuid        primary key default gen_random_uuid(),
    project_id     uuid        not null references projects(id),
    env            text        not null default 'debug' check (env in ('debug', 'release')),
    tags           text[]      not null default '{}',
    username_enc   bytea       not null,
    password_enc   bytea       not null,
    username_hash  text        not null,
    nickname       text        not null,
    status         text        not null default 'idle' check (status in ('idle', 'leased', 'cooling', 'quarantined')),
    cooldown_until timestamptz,
    needs_cleanup  boolean     not null default false,
    created_at     timestamptz not null default now(),
    unique (project_id, env, username_hash)
);

create table account_leases (
    id                     uuid        primary key default gen_random_uuid(),
    account_id             uuid        not null references test_accounts(id),
    run_id                 uuid        not null unique references runs(id),
    device_id              uuid        not null references devices(id),
    acquired_at            timestamptz not null default now(),
    heartbeat_at           timestamptz not null default now(),
    expires_at             timestamptz not null,
    released_at            timestamptz,
    dirty                  boolean     not null default false,
    credentials_fetched_at timestamptz
);

create unique index account_leases_account_lock on account_leases (account_id) where released_at is null;
create index account_leases_expiry_idx on account_leases (expires_at) where released_at is null;

alter table runs add column lease_id uuid references account_leases(id);
