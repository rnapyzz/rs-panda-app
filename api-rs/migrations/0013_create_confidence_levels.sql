-- 確度を % から「段階」に置き換える（docs/plan.md「2.8 確度とリスク」「10. 既存データの移行」）。
--
-- - 確度の段階のマスタ（confidence_levels）を作り、初期値（A〜E）を入れる
-- - 施策の確度（activities.probability）を段階（activities.confidence_level）に置き換える
-- - 金額は満額で持つため、計算式の確度（probability）を取り除く

CREATE TABLE confidence_levels (
    id         BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    code       VARCHAR(10)     NOT NULL COMMENT '段階のコード（例: A）。作成後は変更できない',
    name       VARCHAR(50)     NOT NULL,
    rate       DECIMAL(5,4)    NOT NULL COMMENT '標準の確率（0〜1）',
    criteria   TEXT            NULL COMMENT '判定基準（事実で判断できる文）',
    sort_order INT             NOT NULL DEFAULT 0,
    created_at DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_confidence_levels_code (code),
    CONSTRAINT chk_confidence_levels_rate CHECK (rate >= 0 AND rate <= 1)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='確度の段階';

INSERT INTO confidence_levels (code, name, rate, criteria, sort_order) VALUES
    ('A', '確定', 1.0000, '契約・発注済み、または継続契約中', 1),
    ('B', '高',   0.8000, '内示・口頭合意がある', 2),
    ('C', '中',   0.5000, '見積・提案書を提出済み', 3),
    ('D', '低',   0.2000, '引き合い・初回提案の段階', 4),
    ('E', '構想', 0.0000, '具体的な相手先・計画がない', 5);

ALTER TABLE activities ADD COLUMN confidence_level VARCHAR(10) NULL COMMENT '確度の段階' AFTER owner_user_id;

UPDATE activities SET confidence_level = CASE
    WHEN probability IS NULL THEN IF(activity_type = 'project', 'C', 'A')
    WHEN probability >= 0.9 THEN 'A'
    WHEN probability >= 0.65 THEN 'B'
    WHEN probability >= 0.35 THEN 'C'
    WHEN probability >= 0.1 THEN 'D'
    ELSE 'E'
END;

ALTER TABLE activities
    MODIFY COLUMN confidence_level VARCHAR(10) NOT NULL COMMENT '確度の段階',
    ADD KEY idx_activities_confidence_level (confidence_level),
    ADD CONSTRAINT fk_activities_confidence_level FOREIGN KEY (confidence_level) REFERENCES confidence_levels (code),
    DROP COLUMN probability;

-- 計算式の確度を取り除く（掛け算の因子として使われている場合）。それ以外の使い方が残った式は、反映をやめて直接入力にする
UPDATE activity_lines
SET expression = TRIM(REPLACE(REPLACE(REPLACE(REPLACE(expression, ' * probability', ''), '*probability', ''), 'probability * ', ''), 'probability*', ''))
WHERE expression LIKE '%probability%';
UPDATE activity_lines SET formula_enabled = FALSE WHERE expression LIKE '%probability%';
UPDATE activity_lines SET expression = NULL, formula_enabled = FALSE WHERE expression = '';
