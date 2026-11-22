-- 节点身份密钥：只存 SHA-256 摘要。
-- 为空表示老节点：下次通过 WebSocket 重连时由服务端补发密钥并写入摘要。
alter table nodes add column secret_hash text;
