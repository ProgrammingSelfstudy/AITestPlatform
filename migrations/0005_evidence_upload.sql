-- 节点上传使用 shot / page，已登记的其它文件类型仍可保留。
alter table files drop constraint files_kind_check;
alter table files add constraint files_kind_check
    check (kind in ('screenshot', 'recording', 'trace', 'page_text', 'shot', 'page'));

-- 同一路径只能登记一次。统一比较大小写，防止在大小写不敏感的磁盘上覆盖其它证据。
create unique index files_upload_path_idx on files (run_id, lower(path)) where kind in ('shot', 'page');
