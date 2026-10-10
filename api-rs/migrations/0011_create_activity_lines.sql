-- 金額の内訳（activity_lines）を追加し、施策単位の算出方式（calc_mode）と科目ごとの計算式（activity_formulas）を置き換える。
--
-- 施策 × 科目の下に、名前付きの内訳を複数持てる（例: 売上高 = 月額利用料 + 初期導入費 + スポット売上）。
-- 内訳ごとに、計算式で反映する（formula_enabled）か、直接入力するかを決める。反映しない場合も式は残せる。
-- 科目の金額は、内訳の金額と、科目への直接入力（line_id が NULL の金額）の合計。実績は科目単位（line_id は NULL）。
--
-- 既存のデータは金額が変わらないように移行する:
--   - 計算式 1 件につき内訳を 1 件作る（名前は科目名）。反映の有無は施策の算出方式から引き継ぐ
--   - 算出方式が「計算式」だった施策の、計算式で算出した金額は、その内訳の金額にする

CREATE TABLE activity_lines (
    id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    activity_id     BIGINT UNSIGNED NOT NULL,
    subject_id      BIGINT UNSIGNED NOT NULL,
    name            VARCHAR(100)    NOT NULL COMMENT '内訳名（例: 月額利用料）',
    expression      VARCHAR(1000)   NULL COMMENT '計算式（例: unit_price * customers * probability）',
    formula_enabled BOOLEAN         NOT NULL DEFAULT FALSE COMMENT '計算式で金額を算出して反映するか',
    sort_order      INT             NOT NULL DEFAULT 0,
    created_at      DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_activity_lines_name (activity_id, subject_id, name),
    KEY idx_activity_lines_subject (subject_id),
    CONSTRAINT fk_activity_lines_activity FOREIGN KEY (activity_id) REFERENCES activities (id),
    CONSTRAINT fk_activity_lines_subject  FOREIGN KEY (subject_id)  REFERENCES subjects (id),
    CONSTRAINT chk_activity_lines_formula CHECK (NOT formula_enabled OR expression IS NOT NULL)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='施策×科目の金額の内訳';

INSERT INTO activity_lines (activity_id, subject_id, name, expression, formula_enabled, created_at, updated_at)
SELECT f.activity_id, f.subject_id, s.name, f.expression, a.calc_mode = 'formula', f.created_at, f.updated_at
FROM activity_formulas f
JOIN subjects s ON s.id = f.subject_id
JOIN activities a ON a.id = f.activity_id;

-- 金額に内訳を持たせる。一意制約は NULL（科目への直接入力）を 0 として扱う
ALTER TABLE budget_facts
    ADD COLUMN line_id BIGINT UNSIGNED NULL COMMENT '内訳（NULL は科目への直接入力）' AFTER subject_id,
    ADD COLUMN line_key BIGINT UNSIGNED AS (IFNULL(line_id, 0)) STORED AFTER line_id,
    DROP INDEX uq_budget_facts,
    ADD UNIQUE KEY uq_budget_facts (scenario_id, activity_id, subject_id, line_key, target_month),
    ADD KEY idx_budget_facts_line (line_id),
    ADD CONSTRAINT fk_budget_facts_line FOREIGN KEY (line_id) REFERENCES activity_lines (id);

UPDATE budget_facts b
JOIN activities a ON a.id = b.activity_id
JOIN activity_lines l ON l.activity_id = b.activity_id AND l.subject_id = b.subject_id
SET b.line_id = l.id
WHERE a.calc_mode = 'formula' AND b.source = 'formula';

DROP TABLE activity_formulas;
ALTER TABLE activities DROP COLUMN calc_mode;
