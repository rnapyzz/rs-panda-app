-- 年度の締め（docs/plan.md「2.14 締めた後の実績の修正と年度の締め」）。
-- 締めた年度の月は、実績の取込・再割当・未割当の割当ができない。

CREATE TABLE fiscal_year_closings (
    fiscal_year INT             NOT NULL COMMENT '4月開始の年度',
    closed_at   DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    closed_by   BIGINT UNSIGNED NOT NULL,
    PRIMARY KEY (fiscal_year),
    CONSTRAINT fk_fiscal_year_closings_user FOREIGN KEY (closed_by) REFERENCES users (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='締めた年度';
