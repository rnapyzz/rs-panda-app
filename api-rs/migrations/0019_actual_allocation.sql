-- 実績の割当（docs/plan.md「2.12 実績の割当」「11. 既存データの移行（実績の割当）」）。
--
-- - 会計科目（gl_accounts）: 会計システムの勘定科目と、アプリの科目への対応
-- - 割当ルール（allocation_rules）: 会計科目 × 部門（NULL は全部門）→ 施策
-- - 会計の明細（actual_entries）: 取り込んだ明細と、割り当てた施策・割当の根拠
-- - 実績（actual_facts）・ロックしたシナリオの実績（scenario_actuals）は、施策が NULL の行を未割当として持つ
-- - 会計科目は既存の科目と同じコード・名前で作り、その科目に対応させる

CREATE TABLE gl_accounts (
    id           BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    code         VARCHAR(50)     NOT NULL,
    name         VARCHAR(100)    NOT NULL,
    subject_id   BIGINT UNSIGNED NULL COMMENT 'アプリの科目。対象外なら NULL',
    is_excluded  BOOLEAN         NOT NULL DEFAULT FALSE COMMENT '対象外（P/L に関係ない会計科目）',
    hide_details BOOLEAN         NOT NULL DEFAULT FALSE COMMENT '明細を FP&A 以外に見せない',
    created_at   DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at   DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_gl_accounts_code (code),
    KEY idx_gl_accounts_subject (subject_id),
    CONSTRAINT fk_gl_accounts_subject FOREIGN KEY (subject_id) REFERENCES subjects (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='会計科目（会計システムの勘定科目）';

INSERT INTO gl_accounts (code, name, subject_id)
SELECT code, name, id FROM subjects;

CREATE TABLE allocation_rules (
    id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    gl_account_id   BIGINT UNSIGNED NOT NULL,
    department_code VARCHAR(50)     NULL COMMENT '会計システムの部門コード。NULL は全部門',
    department_key  VARCHAR(50)     AS (IFNULL(department_code, '')) STORED,
    activity_id     BIGINT UNSIGNED NOT NULL,
    created_at      DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_allocation_rules (gl_account_id, department_key),
    KEY idx_allocation_rules_activity (activity_id),
    CONSTRAINT fk_allocation_rules_gl_account FOREIGN KEY (gl_account_id) REFERENCES gl_accounts (id),
    CONSTRAINT fk_allocation_rules_activity   FOREIGN KEY (activity_id)   REFERENCES activities (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='割当ルール（会計科目 × 部門 → 施策）';

CREATE TABLE actual_entries (
    id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    target_month    DATE            NOT NULL COMMENT '月初日',
    line_no         INT             NOT NULL COMMENT '取り込んだ CSV の行番号',
    gl_account_id   BIGINT UNSIGNED NOT NULL,
    subject_id      BIGINT UNSIGNED NOT NULL COMMENT '取込時の会計科目の対応',
    department_code VARCHAR(50)     NULL,
    box_code        VARCHAR(100)    NULL COMMENT '箱の ID（施策コードまたは外部コード）',
    amount          DECIMAL(18,0)   NOT NULL COMMENT '円',
    description     VARCHAR(200)    NULL COMMENT '摘要',
    activity_id     BIGINT UNSIGNED NULL COMMENT 'NULL は未割当',
    allocated_by    ENUM('activity_code', 'external_code', 'rule', 'manual', 'unallocated') NOT NULL,
    change_set_id   BIGINT UNSIGNED NOT NULL COMMENT '取り込んだ変更セット',
    PRIMARY KEY (id),
    KEY idx_actual_entries_month (target_month),
    KEY idx_actual_entries_activity (activity_id, target_month),
    KEY idx_actual_entries_box (box_code),
    KEY idx_actual_entries_account (gl_account_id, department_code),
    KEY idx_actual_entries_subject (subject_id),
    KEY idx_actual_entries_change_set (change_set_id),
    CONSTRAINT fk_actual_entries_gl_account FOREIGN KEY (gl_account_id) REFERENCES gl_accounts (id),
    CONSTRAINT fk_actual_entries_subject    FOREIGN KEY (subject_id)    REFERENCES subjects (id),
    CONSTRAINT fk_actual_entries_activity   FOREIGN KEY (activity_id)   REFERENCES activities (id),
    CONSTRAINT fk_actual_entries_change_set FOREIGN KEY (change_set_id) REFERENCES change_sets (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='会計の明細（実績取込）';

-- 実績は、施策が NULL の行を未割当として持つ。一意キーは NULL を 0 として扱う
ALTER TABLE actual_facts
    ADD KEY idx_actual_facts_activity (activity_id),
    MODIFY activity_id BIGINT UNSIGNED NULL COMMENT 'NULL は未割当',
    ADD COLUMN activity_key BIGINT UNSIGNED AS (IFNULL(activity_id, 0)) STORED AFTER activity_id;
ALTER TABLE actual_facts
    DROP KEY uq_actual_facts,
    ADD UNIQUE KEY uq_actual_facts (activity_key, subject_id, target_month);

ALTER TABLE scenario_actuals
    ADD KEY idx_scenario_actuals_scenario (scenario_id),
    MODIFY activity_id BIGINT UNSIGNED NULL COMMENT 'NULL は未割当',
    ADD COLUMN activity_key BIGINT UNSIGNED AS (IFNULL(activity_id, 0)) STORED AFTER activity_id;
ALTER TABLE scenario_actuals
    DROP KEY uq_scenario_actuals,
    ADD UNIQUE KEY uq_scenario_actuals (scenario_id, activity_key, subject_id, target_month);

-- シナリオの金額（施策に割り当てた分）。未割当は scenario_unallocated で別に返す
CREATE OR REPLACE VIEW scenario_amounts AS
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
WHERE s.is_locked AND sa.activity_id IS NOT NULL
UNION ALL
SELECT s.id, af.activity_id, af.subject_id, NULL, af.target_month, af.amount,
       'actual', 'actual', FALSE, NULL
FROM scenarios s
JOIN actual_facts af
  ON af.target_month BETWEEN MAKEDATE(s.fiscal_year, 1) + INTERVAL 3 MONTH AND s.actual_through
WHERE NOT s.is_locked AND s.actual_through IS NOT NULL AND af.activity_id IS NOT NULL;

-- シナリオの未割当の実績（決算確定月以前の月）
CREATE VIEW scenario_unallocated AS
SELECT sa.scenario_id, sa.subject_id, sa.target_month, sa.amount
FROM scenario_actuals sa
JOIN scenarios s ON s.id = sa.scenario_id
WHERE s.is_locked AND sa.activity_id IS NULL
UNION ALL
SELECT s.id, af.subject_id, af.target_month, af.amount
FROM scenarios s
JOIN actual_facts af
  ON af.target_month BETWEEN MAKEDATE(s.fiscal_year, 1) + INTERVAL 3 MONTH AND s.actual_through
WHERE NOT s.is_locked AND s.actual_through IS NOT NULL AND af.activity_id IS NULL;
