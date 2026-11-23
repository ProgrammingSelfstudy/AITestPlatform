-- 管理员账号：可以强制解除任何节点的绑定。账号和密码不放在迁移或代码里，由运行服务端的人自己建。
alter table users add column is_admin boolean not null default false;
