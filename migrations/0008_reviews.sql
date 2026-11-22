create table reviews (
    id uuid primary key default gen_random_uuid(),
    run_id uuid not null references runs(id) on delete cascade,
    item_idx integer,
    mark text not null check (mark in ('correct', 'passed', 'failed')),
    judge_verdict text not null check (judge_verdict in ('passed', 'failed', 'uncertain')),
    note text check (char_length(note) <= 2000),
    reviewer text not null check (char_length(reviewer) between 1 and 32 and reviewer !~ '[[:cntrl:]]'),
    created_at timestamptz not null default clock_timestamp(),
    foreign key (run_id, item_idx) references judge_items (run_id, idx)
);

create index reviews_current_idx on reviews (run_id, item_idx, created_at desc, id desc);

create view current_reviews as
    select distinct on (run_id, item_idx) *
    from reviews
    order by run_id, item_idx, created_at desc, id desc;
