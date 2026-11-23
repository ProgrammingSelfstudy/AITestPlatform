-- 页面上「创建名字」时设的密码：只存 argon2 哈希。节点启动时登记的名字没有密码（为空）。
alter table users add column password_hash text;
