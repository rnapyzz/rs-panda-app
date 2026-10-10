-- 施策 × シナリオの差異の説明・要因の分類・更新の完了（docs/plan.md「2.10 現場担当の動線」）。
--
-- 状態: completed_at があれば「完了」、なければ last_edited_at があれば「入力中」、どちらもなければ「未着手」。
-- 数値（金額・ドライバー値・想定条件）や説明を変更すると last_edited_at を更新し、完了は取り消す。

CREATE TABLE activity_scenario_notes (
    id             BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    scenario_id    BIGINT UNSIGNED NOT NULL,
    activity_id    BIGINT UNSIGNED NOT NULL,
    explanation    TEXT            NULL COMMENT '差異の説明（基準・前回見込からの差がなぜ生じたか）',
    causes         SET('timing', 'volume', 'new', 'lost', 'assumption', 'other') NOT NULL DEFAULT '' COMMENT '要因の分類',
    completed_at   DATETIME        NULL COMMENT '更新を完了にした日時',
    completed_by   BIGINT UNSIGNED NULL,
    last_edited_at DATETIME        NULL COMMENT '数値・説明を最後に変更した日時',
    created_at     DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at     DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_activity_scenario_notes (scenario_id, activity_id),
    KEY idx_activity_scenario_notes_activity (activity_id),
    CONSTRAINT fk_activity_scenario_notes_scenario FOREIGN KEY (scenario_id) REFERENCES scenarios (id),
    CONSTRAINT fk_activity_scenario_notes_activity FOREIGN KEY (activity_id) REFERENCES activities (id),
    CONSTRAINT fk_activity_scenario_notes_user     FOREIGN KEY (completed_by) REFERENCES users (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='施策 × シナリオの差異の説明と更新の完了';
