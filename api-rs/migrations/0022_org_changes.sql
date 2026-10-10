-- 組織変更と異動（docs/plan.md「2.15 組織変更と異動」）。
--
-- - ユニットの廃止（is_archived）。統合したユニットは廃止にする
-- - 組織変更の予約（org_change_plans）と、その変更（org_change_items）。有効日に自動で適用する
--   変更の対象（施策・ユニット・ノード・ユーザー）は外部キーにしない（予約の後に削除されたら、適用の失敗として扱う）

ALTER TABLE units
    ADD COLUMN is_archived BOOLEAN NOT NULL DEFAULT FALSE COMMENT '廃止（一覧・選択肢に出さない）' AFTER owner_user_id;

CREATE TABLE org_change_plans (
    id             BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    name           VARCHAR(100)    NOT NULL,
    effective_date DATE            NOT NULL COMMENT '有効日（この日の 0:00 に適用する）',
    status         ENUM('scheduled', 'applied', 'failed', 'cancelled') NOT NULL DEFAULT 'scheduled',
    created_by     BIGINT UNSIGNED NOT NULL,
    applied_at     DATETIME        NULL,
    error          TEXT            NULL COMMENT '失敗の理由',
    created_at     DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at     DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_org_change_plans_due (status, effective_date),
    CONSTRAINT fk_org_change_plans_user FOREIGN KEY (created_by) REFERENCES users (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='組織変更の予約';

CREATE TABLE org_change_items (
    id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    plan_id         BIGINT UNSIGNED NOT NULL,
    sort_order      INT             NOT NULL COMMENT '適用する順番',
    kind            ENUM('move_activity', 'move_unit', 'merge_unit', 'change_owner') NOT NULL,
    activity_id     BIGINT UNSIGNED NULL,
    unit_id         BIGINT UNSIGNED NULL,
    target_unit_id  BIGINT UNSIGNED NULL COMMENT '移動先・統合先',
    segment_id      BIGINT UNSIGNED NULL,
    organization_id BIGINT UNSIGNED NULL,
    owner_user_id   BIGINT UNSIGNED NULL COMMENT '変更後の担当者・マネージャー（NULL は未設定）',
    PRIMARY KEY (id),
    KEY idx_org_change_items_plan (plan_id, sort_order),
    CONSTRAINT fk_org_change_items_plan FOREIGN KEY (plan_id) REFERENCES org_change_plans (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='組織変更の予約の変更';
