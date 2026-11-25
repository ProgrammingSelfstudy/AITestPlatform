-- 账号身份：同一项目的测试账号可能有多个身份（普通用户、VIP、主播……）。
-- 身份清单在「参数配置 → 账号身份」里维护，这里只存身份编码（可空；空 = 不区分身份）。
alter table test_accounts add column identity text;
-- 用例指定要用哪个身份的账号；空 = 按原规则选任意账号，老用例不受影响。
alter table test_cases add column account_identity text;
-- run 创建时把用例的身份要求固化下来，和 case_spec 一样不受之后改用例的影响。
alter table runs add column account_identity text;

create index test_accounts_identity on test_accounts (project_id, env, identity) where identity is not null;

insert into config_types (name, code, remark)
values ('账号身份', 'account_identity', '测试账号的身份清单：账号勾选身份，用例按身份选号（编码即身份标识，不要放账号密码）');
