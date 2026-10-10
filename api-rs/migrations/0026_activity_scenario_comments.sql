-- 説明へのコメント（docs/plan.md「2.24」）。施策 × シナリオの今回の見込の説明に、関係者がコメントを書いてやり取りする。
-- コメントは値ではないので、変更セット・監査ログには残さない。消したコメントは deleted_at を入れ、本文を空にする。

CREATE TABLE activity_scenario_comments (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    scenario_id BIGINT UNSIGNED NOT NULL,
    activity_id BIGINT UNSIGNED NOT NULL,
    user_id     BIGINT UNSIGNED NOT NULL COMMENT '書いた人',
    body        TEXT            NOT NULL COMMENT '1,000 文字まで。消したら空',
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at  DATETIME        NULL COMMENT 'NULL は消していない',
    PRIMARY KEY (id),
    KEY idx_activity_scenario_comments_target (scenario_id, activity_id, id),
    KEY idx_activity_scenario_comments_activity (activity_id),
    CONSTRAINT fk_activity_scenario_comments_scenario FOREIGN KEY (scenario_id) REFERENCES scenarios (id) ON DELETE CASCADE,
    CONSTRAINT fk_activity_scenario_comments_activity FOREIGN KEY (activity_id) REFERENCES activities (id) ON DELETE CASCADE,
    CONSTRAINT fk_activity_scenario_comments_user     FOREIGN KEY (user_id)     REFERENCES users (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='説明へのコメント';
