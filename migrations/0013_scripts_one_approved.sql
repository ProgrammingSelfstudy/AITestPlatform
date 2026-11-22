-- 同一用例同时只能有一个获准的脚本版本：平台按「用例 → 获准版本」选脚本回归，两个获准版本会让选择有歧义。
-- 状态机里也会先检查并给出中文原因；这里是并发时的最后一道保证。
create unique index scripts_one_approved_per_case on scripts (case_id) where status = 'approved';
