-- 生成用例的复核：评审员（模型）先给意见，最后由人确认。评审员的「通过」只是意见，不会让用例直接能执行。
alter table test_cases add column review_note text not null default '' check (char_length(review_note) <= 600);
alter table test_cases add column reviewed_by_user uuid references users(id);
alter table test_cases add column reviewed_at timestamptz;

alter table casegen_requests add column review_detail text not null default '' check (char_length(review_detail) <= 2000);
