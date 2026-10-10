-- 認証: ユーザーの有効/無効とログインセッション

ALTER TABLE users
    ADD COLUMN is_active BOOLEAN NOT NULL DEFAULT TRUE COMMENT '無効化されたユーザーはログインできない' AFTER role;

CREATE TABLE sessions (
    id         CHAR(64)        NOT NULL COMMENT 'セッショントークンの SHA-256（16進）。トークン自体は保存しない',
    user_id    BIGINT UNSIGNED NOT NULL,
    expires_at DATETIME        NOT NULL,
    created_at DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_sessions_user (user_id),
    KEY idx_sessions_expires (expires_at),
    CONSTRAINT fk_sessions_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='ログインセッション';
