-- 参数配置：一级是配置类型，二级是该类型下的配置项。
-- 配置值里不允许出现密码或 API Key（写入接口在 P2 做校验）。
create table config_types (
    id          bigserial primary key,
    name        text        not null,
    code        text        not null unique,
    remark      text        not null default '',
    status      text        not null default 'normal' check (status in ('normal', 'disabled')),
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    updated_by  text
);

create table config_items (
    id          bigserial primary key,
    type_id     bigint      not null references config_types(id) on delete restrict,
    name        text        not null,
    code        text        not null,
    value       text        not null,
    label       text        not null default '',
    group_name  text        not null default '',
    sort        integer     not null default 0,
    remark      text        not null default '',
    status      text        not null default 'normal' check (status in ('normal', 'disabled')),
    updated_at  timestamptz not null default now(),
    updated_by  text,
    unique (type_id, code)
);

-- 种子数据：只放代码里已经存在的真实取值，P1 只展示，还没有接到运行逻辑上。
insert into config_types (name, code, remark) values
    ('岗位角色上限', 'role_limit', '一个节点上每个岗位最多配几个角色'),
    ('设备命令重试', 'device_retry', '设备命令失败、dump 为空时的重试上限（项目规则：不超过 3 次）'),
    ('卡住检测', 'stuck_detect', '设备 MCP 服务里的卡住检测阈值');

insert into config_items (type_id, name, code, value, label, group_name, sort, remark)
select t.id, v.name, v.code, v.value, v.label, v.grp, v.sort, v.remark
from config_types t
join (values
    ('role_limit',    '执行员',       'executor',    '3', '个', 'role',   1, '执行员岗位最多角色数'),
    ('role_limit',    '判定员',       'judge',       '3', '个', 'role',   2, '判定员岗位最多角色数'),
    ('role_limit',    '测试用例',     'case_writer', '3', '个', 'role',   3, '测试用例岗位最多角色数'),
    ('role_limit',    '管理员',       'planner',     '3', '个', 'role',   4, '管理员岗位最多角色数'),
    ('device_retry',  '最大重试次数', 'max_attempts','3', '次', 'device', 1, '含首次尝试；失败时把最后一次原始错误原样返回'),
    ('stuck_detect',  '重复判卡住',   'stuck_n',     '3', '次', 'guard',  1, '连续多少次页面不变或动作重复判定卡住'),
    ('stuck_detect',  '保留记录数',   'keep',        '6', '条', 'guard',  2, '记住最近多少个页面签名和动作，方便排查')
) as v(type_code, name, code, value, label, grp, sort, remark) on v.type_code = t.code;
