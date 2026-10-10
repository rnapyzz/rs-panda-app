-- 重点施策（共有の印）とウォッチ（人ごとの気になる施策の印）（docs/plan.md「2.11 マネージャーの動線」）。

ALTER TABLE activities ADD COLUMN is_priority BOOLEAN NOT NULL DEFAULT FALSE COMMENT '重点施策' AFTER assumptions;

CREATE TABLE activity_watches (
    user_id     BIGINT UNSIGNED NOT NULL,
    activity_id BIGINT UNSIGNED NOT NULL,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, activity_id),
    KEY idx_activity_watches_activity (activity_id),
    CONSTRAINT fk_activity_watches_user     FOREIGN KEY (user_id)     REFERENCES users (id),
    CONSTRAINT fk_activity_watches_activity FOREIGN KEY (activity_id) REFERENCES activities (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='人ごとのウォッチ（気になる施策）';
