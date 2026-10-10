-- 変更履歴: 変更セット（理由）と監査ログ（変更前後の値）

CREATE TABLE change_sets (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    user_id     BIGINT UNSIGNED NOT NULL,
    scenario_id BIGINT UNSIGNED NULL COMMENT 'マスタ変更などシナリオに紐づかない場合はNULL',
    reason      TEXT            NOT NULL,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_change_sets_user (user_id),
    KEY idx_change_sets_scenario (scenario_id, created_at),
    CONSTRAINT fk_change_sets_user     FOREIGN KEY (user_id)     REFERENCES users (id),
    CONSTRAINT fk_change_sets_scenario FOREIGN KEY (scenario_id) REFERENCES scenarios (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='変更セット';

CREATE TABLE audit_logs (
    id            BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    change_set_id BIGINT UNSIGNED NOT NULL,
    table_name    VARCHAR(64)     NOT NULL,
    record_id     BIGINT UNSIGNED NOT NULL,
    action        ENUM('insert', 'update', 'delete') NOT NULL,
    before_json   JSON            NULL,
    after_json    JSON            NULL,
    created_at    DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_audit_logs_change_set (change_set_id),
    KEY idx_audit_logs_record (table_name, record_id),
    CONSTRAINT fk_audit_logs_change_set FOREIGN KEY (change_set_id) REFERENCES change_sets (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='監査ログ';
