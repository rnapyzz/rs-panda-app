-- 施策と、施策に紐づくマイルストーン・ドライバー定義・計算式

CREATE TABLE activities (
    id            BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    function_id   BIGINT UNSIGNED NOT NULL,
    code          VARCHAR(50)     NOT NULL COMMENT '施策コード（CSV取込で使用）',
    name          VARCHAR(200)    NOT NULL,
    activity_type ENUM('project', 'recurring', 'cost_pool') NOT NULL,
    status        VARCHAR(30)     NOT NULL,
    start_date    DATE            NULL,
    end_date      DATE            NULL COMMENT '運用型はNULL可',
    owner_user_id BIGINT UNSIGNED NULL,
    calc_mode     ENUM('formula', 'manual') NOT NULL DEFAULT 'manual',
    probability   DECIMAL(5, 4)   NULL COMMENT '確度（0〜1）',
    assumptions   TEXT            NULL COMMENT '前提条件',
    created_at    DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_activities_code (code),
    KEY idx_activities_function (function_id),
    KEY idx_activities_owner (owner_user_id),
    CONSTRAINT fk_activities_function FOREIGN KEY (function_id)   REFERENCES functions (id),
    CONSTRAINT fk_activities_owner    FOREIGN KEY (owner_user_id) REFERENCES users (id),
    CONSTRAINT chk_activities_probability CHECK (probability IS NULL OR (probability >= 0 AND probability <= 1)),
    CONSTRAINT chk_activities_period      CHECK (start_date IS NULL OR end_date IS NULL OR start_date <= end_date)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='施策マスタ';

CREATE TABLE activity_milestones (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    activity_id BIGINT UNSIGNED NOT NULL,
    name        VARCHAR(200)    NOT NULL,
    due_date    DATE            NOT NULL,
    status      VARCHAR(30)     NOT NULL,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_activity_milestones_activity (activity_id),
    CONSTRAINT fk_activity_milestones_activity FOREIGN KEY (activity_id) REFERENCES activities (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='施策のマイルストーン';

CREATE TABLE activity_drivers (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    activity_id BIGINT UNSIGNED NOT NULL,
    code        VARCHAR(50)     NOT NULL COMMENT '計算式で参照する識別子',
    name        VARCHAR(100)    NOT NULL,
    driver_kind ENUM('value', 'cost', 'kpi') NOT NULL,
    unit        VARCHAR(30)     NULL,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_activity_drivers_code (activity_id, code),
    CONSTRAINT fk_activity_drivers_activity FOREIGN KEY (activity_id) REFERENCES activities (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='施策のドライバー定義';

CREATE TABLE activity_formulas (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    activity_id BIGINT UNSIGNED NOT NULL,
    subject_id  BIGINT UNSIGNED NOT NULL,
    expression  VARCHAR(1000)   NOT NULL COMMENT '例: unit_price * volume * probability',
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_activity_formulas (activity_id, subject_id),
    KEY idx_activity_formulas_subject (subject_id),
    CONSTRAINT fk_activity_formulas_activity FOREIGN KEY (activity_id) REFERENCES activities (id),
    CONSTRAINT fk_activity_formulas_subject  FOREIGN KEY (subject_id)  REFERENCES subjects (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='施策×科目の計算式';
