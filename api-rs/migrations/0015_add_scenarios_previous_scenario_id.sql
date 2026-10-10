-- シナリオの前回見込（docs/plan.md「2.6 シナリオ」）。数値入力画面の比較やホームで「前回締めた見込」として使う。
-- FP&A が指定する。既存のシナリオは複製元を前回見込にする。

ALTER TABLE scenarios
    ADD COLUMN previous_scenario_id BIGINT UNSIGNED NULL COMMENT '前回見込（同じ年度のシナリオ）' AFTER base_scenario_id,
    ADD CONSTRAINT fk_scenarios_previous FOREIGN KEY (previous_scenario_id) REFERENCES scenarios (id);

UPDATE scenarios SET previous_scenario_id = base_scenario_id WHERE base_scenario_id IS NOT NULL;
