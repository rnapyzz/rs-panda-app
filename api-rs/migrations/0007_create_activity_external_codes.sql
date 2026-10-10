-- 外部コード（会計・基幹システムの案件番号など）。施策に0個以上。
-- 1つの外部コードは1つの施策にだけ紐づく。枠の施策には複数の外部コードを付けて実績を受ける。

CREATE TABLE activity_external_codes (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    activity_id BIGINT UNSIGNED NOT NULL,
    code        VARCHAR(100)    NOT NULL COMMENT '外部システムの案件番号など',
    note        VARCHAR(200)    NULL COMMENT 'メモ（例: A社 保守契約）',
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_activity_external_codes_code (code),
    KEY idx_activity_external_codes_activity (activity_id),
    CONSTRAINT fk_activity_external_codes_activity FOREIGN KEY (activity_id) REFERENCES activities (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='施策の外部コード';
