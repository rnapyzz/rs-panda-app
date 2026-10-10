-- マスタ系テーブル: ユーザー・組織・セグメント・機能・勘定科目

CREATE TABLE users (
    id            BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    name          VARCHAR(100)    NOT NULL,
    email         VARCHAR(255)    NOT NULL,
    password_hash VARCHAR(255)    NOT NULL,
    role          ENUM('fpa_admin', 'manager', 'member', 'viewer') NOT NULL,
    created_at    DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_users_email (email)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='ユーザー';

CREATE TABLE organizations (
    id         BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    parent_id  BIGINT UNSIGNED NULL,
    name       VARCHAR(100)    NOT NULL,
    level      INT UNSIGNED    NOT NULL COMMENT 'ルートを1とする階層レベル',
    sort_order INT             NOT NULL DEFAULT 0,
    created_at DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_organizations_parent (parent_id),
    CONSTRAINT fk_organizations_parent FOREIGN KEY (parent_id) REFERENCES organizations (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='組織マスタ';

CREATE TABLE segments (
    id         BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    parent_id  BIGINT UNSIGNED NULL,
    name       VARCHAR(100)    NOT NULL,
    level      INT UNSIGNED    NOT NULL COMMENT 'ルートを1とする階層レベル',
    sort_order INT             NOT NULL DEFAULT 0,
    created_at DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_segments_parent (parent_id),
    CONSTRAINT fk_segments_parent FOREIGN KEY (parent_id) REFERENCES segments (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='事業（セグメント）マスタ';

-- segment_id / organization_id には末端ノードのみ指定可（アプリ側で検証する）
CREATE TABLE functions (
    id              BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    name            VARCHAR(100)    NOT NULL,
    segment_id      BIGINT UNSIGNED NOT NULL,
    organization_id BIGINT UNSIGNED NOT NULL,
    owner_user_id   BIGINT UNSIGNED NULL,
    created_at      DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_functions_segment (segment_id),
    KEY idx_functions_organization (organization_id),
    KEY idx_functions_owner (owner_user_id),
    CONSTRAINT fk_functions_segment      FOREIGN KEY (segment_id)      REFERENCES segments (id),
    CONSTRAINT fk_functions_organization FOREIGN KEY (organization_id) REFERENCES organizations (id),
    CONSTRAINT fk_functions_owner        FOREIGN KEY (owner_user_id)   REFERENCES users (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='機能マスタ（施策を束ねる単位）';

CREATE TABLE subjects (
    id         BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    parent_id  BIGINT UNSIGNED NULL,
    code       VARCHAR(50)     NOT NULL,
    name       VARCHAR(100)    NOT NULL,
    category   ENUM('revenue', 'expense') NOT NULL,
    sort_order INT             NOT NULL DEFAULT 0,
    created_at DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_subjects_code (code),
    KEY idx_subjects_parent (parent_id),
    CONSTRAINT fk_subjects_parent FOREIGN KEY (parent_id) REFERENCES subjects (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='勘定科目マスタ';
