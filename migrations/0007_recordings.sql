alter table run_steps add column at_ms bigint;

alter table files
    add column start_epoch_ms bigint,
    add column duration_ms bigint;

drop index files_upload_path_idx;
create unique index files_upload_path_idx on files (run_id, lower(path))
    where kind in ('shot', 'page', 'recording');
