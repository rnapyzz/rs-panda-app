-- 締切と通知（docs/plan.md「2.13 締切と通知」）。
--
-- - シナリオに現場の更新の締切日（update_deadline）を持たせる
-- - ユーザーに Slack のメンバー ID（slack_user_id）を持たせる
-- - アプリ内のお知らせ（notifications）、送信の記録（notification_runs）、通知の設定（notification_settings、1行）

ALTER TABLE scenarios
    ADD COLUMN update_deadline DATE NULL COMMENT '現場の更新の締切日' AFTER actual_through;

ALTER TABLE users
    ADD COLUMN slack_user_id VARCHAR(20) NULL COMMENT 'Slack のメンバー ID（メンション用）' AFTER is_active;

CREATE TABLE notifications (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    user_id     BIGINT UNSIGNED NOT NULL,
    kind        VARCHAR(30)     NOT NULL COMMENT 'update_started / deadline_reminder / deadline_overdue / actuals_reflected',
    scenario_id BIGINT UNSIGNED NULL,
    title       VARCHAR(200)    NOT NULL,
    body        TEXT            NOT NULL,
    link        VARCHAR(200)    NOT NULL,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    read_at     DATETIME        NULL COMMENT 'NULL は未読',
    PRIMARY KEY (id),
    KEY idx_notifications_user (user_id, created_at),
    KEY idx_notifications_created (created_at),
    CONSTRAINT fk_notifications_user     FOREIGN KEY (user_id)     REFERENCES users (id),
    CONSTRAINT fk_notifications_scenario FOREIGN KEY (scenario_id) REFERENCES scenarios (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='アプリ内のお知らせ';

CREATE TABLE notification_runs (
    id             BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    kind           VARCHAR(30)     NOT NULL,
    scenario_id    BIGINT UNSIGNED NOT NULL,
    run_date       DATE            NOT NULL COMMENT '送信した日（日本時間）',
    recipients     INT             NOT NULL DEFAULT 0 COMMENT '宛先の人数',
    slack_status   ENUM('skipped', 'sent', 'failed') NOT NULL DEFAULT 'skipped',
    slack_attempts INT             NOT NULL DEFAULT 0,
    slack_error    TEXT            NULL,
    slack_text     TEXT            NULL COMMENT '送り直し用の Slack の本文',
    created_at     DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at     DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_notification_runs (kind, scenario_id, run_date),
    KEY idx_notification_runs_scenario (scenario_id),
    CONSTRAINT fk_notification_runs_scenario FOREIGN KEY (scenario_id) REFERENCES scenarios (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='通知の送信の記録';

CREATE TABLE notification_settings (
    id            TINYINT UNSIGNED NOT NULL,
    enabled_kinds JSON             NOT NULL COMMENT '種類 → 有効か',
    reminder_days VARCHAR(50)      NOT NULL COMMENT '締切の何日前に知らせるか（カンマ区切り）',
    send_time     TIME             NOT NULL COMMENT '送信時刻（日本時間）',
    updated_at    DATETIME         NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    CONSTRAINT chk_notification_settings_single CHECK (id = 1)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='通知の設定（1行）';

INSERT INTO notification_settings (id, enabled_kinds, reminder_days, send_time) VALUES
    (1, JSON_OBJECT('update_started', TRUE, 'deadline_reminder', TRUE, 'deadline_overdue', TRUE, 'actuals_reflected', TRUE), '3,1', '09:00:00');
