-- 身份改成「只记名字」：用户由节点启动时的 NODE_USER_NAME 登记，不再需要个人令牌。
-- 老用户（命令行创建、带令牌）保留；新用户没有令牌，所以令牌摘要改成可空（唯一约束对空值不生效）。
alter table users alter column token_hash drop not null;
