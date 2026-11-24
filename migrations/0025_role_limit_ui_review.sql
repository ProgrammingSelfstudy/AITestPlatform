-- 新增岗位「UI 走查」（ui_review）：在岗位角色上限里补一项，默认每个节点最多 3 个。
insert into config_items (type_id, name, code, value, label, group_name, sort, remark)
select t.id, 'UI 走查', 'ui_review', '3', '个', 'role', 5, 'UI 走查岗位最多角色数'
from config_types t where t.code = 'role_limit'
on conflict (type_id, code) do nothing;
