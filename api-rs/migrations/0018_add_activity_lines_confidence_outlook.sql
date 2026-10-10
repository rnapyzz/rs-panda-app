-- 内訳の確度の段階と見通しの種類（docs/plan.md「2.8 確度とリスク」）。
-- 段階が NULL の内訳は施策の段階を使う。見通しの種類の既定はベース。

ALTER TABLE activity_lines
    ADD COLUMN confidence_level VARCHAR(10) NULL COMMENT '確度の段階（NULL は施策の段階）' AFTER formula_enabled,
    ADD COLUMN outlook ENUM('base', 'addon', 'downside') NOT NULL DEFAULT 'base' COMMENT '見通しの種類（ベース・アドオン・ダウンサイド）' AFTER confidence_level,
    ADD KEY idx_activity_lines_confidence_level (confidence_level),
    ADD CONSTRAINT fk_activity_lines_confidence_level FOREIGN KEY (confidence_level) REFERENCES confidence_levels (code);
