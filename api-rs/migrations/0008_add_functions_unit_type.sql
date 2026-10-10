-- ユニット（functions）に種別を追加する。既存のユニットはサービスとする。
--   service     : サービス（プロフィットセンター）
--   cost_center : 共通費（コストセンター）。例: ○○事業共通経費
--   corporate   : 管理部門。例: 人事部、経理部

ALTER TABLE functions
    ADD COLUMN unit_type ENUM('service', 'cost_center', 'corporate') NOT NULL DEFAULT 'service' COMMENT 'ユニットの種別' AFTER name;
