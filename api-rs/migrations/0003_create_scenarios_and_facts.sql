-- シナリオと、シナリオごとの数値（ドライバー値・金額）

CREATE TABLE scenarios (
    id               BIGINT UNSIGNED   NOT NULL AUTO_INCREMENT,
    name             VARCHAR(100)      NOT NULL,
    scenario_kind    ENUM('budget', 'forecast', 'actual', 'optimistic', 'pessimistic', 'other') NOT NULL,
    fiscal_year      SMALLINT UNSIGNED NOT NULL COMMENT '4月開始の年度（2026 = 2026-04〜2027-03）',
    base_scenario_id BIGINT UNSIGNED   NULL COMMENT '複製元シナリオ',
    is_locked        BOOLEAN           NOT NULL DEFAULT FALSE,
    created_by       BIGINT UNSIGNED   NOT NULL,
    created_at       DATETIME          NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at       DATETIME          NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_scenarios_name (name),
    KEY idx_scenarios_base (base_scenario_id),
    KEY idx_scenarios_created_by (created_by),
    CONSTRAINT fk_scenarios_base       FOREIGN KEY (base_scenario_id) REFERENCES scenarios (id),
    CONSTRAINT fk_scenarios_created_by FOREIGN KEY (created_by)       REFERENCES users (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='シナリオマスタ';

CREATE TABLE scenario_conditions (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    scenario_id BIGINT UNSIGNED NOT NULL,
    activity_id BIGINT UNSIGNED NOT NULL,
    description TEXT            NOT NULL COMMENT '想定内容と発生条件',
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_scenario_conditions (scenario_id, activity_id),
    KEY idx_scenario_conditions_activity (activity_id),
    CONSTRAINT fk_scenario_conditions_scenario FOREIGN KEY (scenario_id) REFERENCES scenarios (id),
    CONSTRAINT fk_scenario_conditions_activity FOREIGN KEY (activity_id) REFERENCES activities (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='シナリオ×施策の想定内容と発生条件';

CREATE TABLE driver_values (
    id                 BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    activity_driver_id BIGINT UNSIGNED NOT NULL,
    scenario_id        BIGINT UNSIGNED NOT NULL,
    target_month       DATE            NOT NULL COMMENT '月初日',
    value              DECIMAL(24, 6)  NOT NULL,
    is_provisional     BOOLEAN         NOT NULL DEFAULT FALSE COMMENT '仮の値か',
    provisional_reason TEXT            NULL,
    created_at         DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at         DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_driver_values (activity_driver_id, scenario_id, target_month),
    KEY idx_driver_values_scenario (scenario_id, target_month),
    CONSTRAINT fk_driver_values_driver   FOREIGN KEY (activity_driver_id) REFERENCES activity_drivers (id),
    CONSTRAINT fk_driver_values_scenario FOREIGN KEY (scenario_id)        REFERENCES scenarios (id),
    CONSTRAINT chk_driver_values_month CHECK (DAY(target_month) = 1)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='ドライバーの月次値';

CREATE TABLE budget_facts (
    id                 BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    scenario_id        BIGINT UNSIGNED NOT NULL,
    activity_id        BIGINT UNSIGNED NOT NULL,
    subject_id         BIGINT UNSIGNED NOT NULL,
    target_month       DATE            NOT NULL COMMENT '月初日',
    amount             DECIMAL(18, 0)  NOT NULL COMMENT '円',
    source             ENUM('manual', 'formula', 'import') NOT NULL,
    is_provisional     BOOLEAN         NOT NULL DEFAULT FALSE COMMENT '仮の値か',
    provisional_reason TEXT            NULL,
    created_at         DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at         DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_budget_facts (scenario_id, activity_id, subject_id, target_month),
    KEY idx_budget_facts_scenario_month (scenario_id, target_month),
    KEY idx_budget_facts_activity (activity_id),
    KEY idx_budget_facts_subject (subject_id),
    CONSTRAINT fk_budget_facts_scenario FOREIGN KEY (scenario_id) REFERENCES scenarios (id),
    CONSTRAINT fk_budget_facts_activity FOREIGN KEY (activity_id) REFERENCES activities (id),
    CONSTRAINT fk_budget_facts_subject  FOREIGN KEY (subject_id)  REFERENCES subjects (id),
    CONSTRAINT chk_budget_facts_month CHECK (DAY(target_month) = 1)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='金額ファクト';
