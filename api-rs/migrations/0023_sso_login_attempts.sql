-- 本番運用（docs/architecture.md「認証」）。
--
-- - SSO（OIDC）のアカウント ID（sub）。初回の SSO ログインで記録し、以後も照合する
-- - パスワードでのログインの失敗の記録（ログイン失敗の制限に使う）

ALTER TABLE users
    ADD COLUMN oidc_subject VARCHAR(255) NULL COMMENT 'SSO のアカウント ID（sub）' AFTER slack_user_id,
    ADD UNIQUE KEY uq_users_oidc_subject (oidc_subject);

CREATE TABLE login_attempts (
    id           BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    email        VARCHAR(255)    NOT NULL,
    ip           VARCHAR(45)     NOT NULL,
    attempted_at DATETIME(3)     NOT NULL,
    PRIMARY KEY (id),
    KEY idx_login_attempts_email (email, attempted_at),
    KEY idx_login_attempts_ip (ip, attempted_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='パスワードでのログインの失敗';
