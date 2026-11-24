-- 模块的 UI 图：按模块管理，可以单独上传、删除。
-- 以前截图只挂在「用例生成请求」上，页面看不到也删不掉；这里把它们回填成模块的图，之后生成用例时上传的也会进来。
create table module_screenshots (
    id           uuid primary key default gen_random_uuid(),
    module_id    uuid        not null references modules(id) on delete cascade,
    name         text        not null check (char_length(name) between 1 and 120),
    content_type text        not null check (content_type in ('image/png', 'image/jpeg')),
    bytes        bytea       not null,
    sha256       text        not null,
    created_by   uuid        references users(id) on delete set null,
    created_at   timestamptz not null default now(),
    -- 同一个模块里同一张图只存一份
    unique (module_id, sha256)
);
create index module_screenshots_module_idx on module_screenshots (module_id, created_at);

insert into module_screenshots (module_id, name, content_type, bytes, sha256, created_by, created_at)
select distinct on (r.module_id, s.sha256)
       r.module_id, '生成用例时上传的截图 ' || (s.seq + 1)::text, s.content_type, s.bytes, s.sha256, r.requested_by, r.requested_at
from casegen_screenshots s
join casegen_requests r on r.id = s.request_id
order by r.module_id, s.sha256, r.requested_at
on conflict (module_id, sha256) do nothing;
