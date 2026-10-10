-- 閲覧の制限（docs/plan.md「2.17」）。
--
-- - 閲覧制限のある科目の金額は、FP&A と経営陣・レビュアーだけが見られる。既存の科目は制限なしで始める

ALTER TABLE subjects
    ADD COLUMN is_restricted BOOLEAN NOT NULL DEFAULT FALSE COMMENT '閲覧制限（FP&A と経営陣のみ金額を見られる）' AFTER sort_order;
