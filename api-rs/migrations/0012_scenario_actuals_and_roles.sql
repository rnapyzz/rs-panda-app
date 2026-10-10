-- シナリオの仕様の見直し（docs/plan.md「2.6 シナリオ」「9. 既存データの移行」）。
--
-- - 実績をシナリオから切り離し、実績データ（actual_facts）にする
-- - シナリオは決算確定月（actual_through）を持ち、それ以前の月は実績、それより後の月は計画値とする
--   ロック時に決算確定月以前の実績を scenario_actuals に保存して固定する
-- - 年度ごとのエイリアス（plan_role: 期初計画・修正計画・最新見込）と、作成中（is_active、アプリ全体で1つ）を追加する
-- - シナリオの種別（scenario_kind）を廃止する
-- - シナリオの金額（実績と計画値の組み合わせ）を返すビュー scenario_amounts を作る

CREATE TABLE actual_facts (
    id           BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    activity_id  BIGINT UNSIGNED NOT NULL,
    subject_id   BIGINT UNSIGNED NOT NULL,
    target_month DATE            NOT NULL COMMENT '月初日',
    amount       DECIMAL(18,0)   NOT NULL COMMENT '円',
    created_at   DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at   DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_actual_facts (activity_id, subject_id, target_month),
    KEY idx_actual_facts_month (target_month),
    KEY idx_actual_facts_subject (subject_id),
    CONSTRAINT fk_actual_facts_activity FOREIGN KEY (activity_id) REFERENCES activities (id),
    CONSTRAINT fk_actual_facts_subject  FOREIGN KEY (subject_id)  REFERENCES subjects (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='実績（施策×科目×月）';

CREATE TABLE scenario_actuals (
    id           BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    scenario_id  BIGINT UNSIGNED NOT NULL,
    activity_id  BIGINT UNSIGNED NOT NULL,
    subject_id   BIGINT UNSIGNED NOT NULL,
    target_month DATE            NOT NULL COMMENT '月初日',
    amount       DECIMAL(18,0)   NOT NULL COMMENT '円',
    created_at   DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_scenario_actuals (scenario_id, activity_id, subject_id, target_month),
    KEY idx_scenario_actuals_activity (activity_id),
    KEY idx_scenario_actuals_subject (subject_id),
    CONSTRAINT fk_scenario_actuals_scenario FOREIGN KEY (scenario_id) REFERENCES scenarios (id),
    CONSTRAINT fk_scenario_actuals_activity FOREIGN KEY (activity_id) REFERENCES activities (id),
    CONSTRAINT fk_scenario_actuals_subject  FOREIGN KEY (subject_id)  REFERENCES subjects (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='ロックしたシナリオに保存した、決算確定月以前の実績';

-- 実績シナリオの金額を実績データに移す。同じ年度に実績シナリオが複数ある場合は ID が大きいものを使う
INSERT INTO actual_facts (activity_id, subject_id, target_month, amount)
SELECT b.activity_id, b.subject_id, b.target_month, SUM(b.amount)
FROM budget_facts b
JOIN scenarios s ON s.id = b.scenario_id
JOIN (SELECT fiscal_year, MAX(id) AS id FROM scenarios WHERE scenario_kind = 'actual' GROUP BY fiscal_year) latest ON latest.id = s.id
GROUP BY b.activity_id, b.subject_id, b.target_month;

ALTER TABLE scenarios
    ADD COLUMN plan_role ENUM('initial', 'revised', 'latest') NULL COMMENT 'エイリアス（期初計画・修正計画・最新見込）' AFTER fiscal_year,
    ADD COLUMN actual_through DATE NULL COMMENT '決算確定月（月初日）。この月以前は実績' AFTER plan_role,
    ADD COLUMN is_active BOOLEAN NOT NULL DEFAULT FALSE COMMENT '作成中のシナリオ' AFTER actual_through,
    ADD COLUMN active_key TINYINT AS (IF(is_active, 1, NULL)) STORED,
    ADD UNIQUE KEY uq_scenarios_plan_role (fiscal_year, plan_role),
    ADD UNIQUE KEY uq_scenarios_active (active_key);

-- 年度ごとに、最初の予算を期初計画、最後の見込を最新見込にする
UPDATE scenarios s
JOIN (SELECT fiscal_year, MIN(id) AS id FROM scenarios WHERE scenario_kind = 'budget' GROUP BY fiscal_year) t ON t.id = s.id
SET s.plan_role = 'initial';
UPDATE scenarios s
JOIN (SELECT fiscal_year, MAX(id) AS id FROM scenarios WHERE scenario_kind = 'forecast' GROUP BY fiscal_year) t ON t.id = s.id
SET s.plan_role = 'latest';

-- 実績シナリオは、ロック済みの通常のシナリオとして残す
UPDATE scenarios SET is_locked = TRUE WHERE scenario_kind = 'actual';

ALTER TABLE scenarios DROP COLUMN scenario_kind;

-- シナリオの金額。決算確定月以前の月は実績（ロック済みは scenario_actuals、それ以外は actual_facts）、
-- それより後の月は計画値（budget_facts）。kind は 'plan' / 'actual'。実績は内訳を持たない（line_id は NULL）。
CREATE VIEW scenario_amounts AS
SELECT b.scenario_id, b.activity_id, b.subject_id, b.line_id, b.target_month, b.amount,
       'plan' AS kind, b.source, b.is_provisional, b.provisional_reason
FROM budget_facts b
JOIN scenarios s ON s.id = b.scenario_id
WHERE s.actual_through IS NULL OR b.target_month > s.actual_through
UNION ALL
SELECT sa.scenario_id, sa.activity_id, sa.subject_id, NULL, sa.target_month, sa.amount,
       'actual', 'actual', FALSE, NULL
FROM scenario_actuals sa
JOIN scenarios s ON s.id = sa.scenario_id
WHERE s.is_locked
UNION ALL
SELECT s.id, af.activity_id, af.subject_id, NULL, af.target_month, af.amount,
       'actual', 'actual', FALSE, NULL
FROM scenarios s
JOIN actual_facts af
  ON af.target_month BETWEEN MAKEDATE(s.fiscal_year, 1) + INTERVAL 3 MONTH AND s.actual_through
WHERE NOT s.is_locked AND s.actual_through IS NOT NULL;
