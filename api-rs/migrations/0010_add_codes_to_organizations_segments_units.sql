-- 組織・セグメント・ユニットにコードを追加する（CSV の取込で行と既存データを結びつけるキー）。
-- 既存のデータには ORG-0001 / SEG-0001 / UNIT-0001 の形式で ID 順に採番する。

ALTER TABLE organizations ADD COLUMN code VARCHAR(50) NULL AFTER parent_id;
ALTER TABLE segments      ADD COLUMN code VARCHAR(50) NULL AFTER parent_id;
ALTER TABLE units         ADD COLUMN code VARCHAR(50) NULL AFTER id;

UPDATE organizations SET code = CONCAT('ORG-',  LPAD(id, 4, '0'));
UPDATE segments      SET code = CONCAT('SEG-',  LPAD(id, 4, '0'));
UPDATE units         SET code = CONCAT('UNIT-', LPAD(id, 4, '0'));

ALTER TABLE organizations MODIFY COLUMN code VARCHAR(50) NOT NULL COMMENT '組織コード', ADD UNIQUE KEY uq_organizations_code (code);
ALTER TABLE segments      MODIFY COLUMN code VARCHAR(50) NOT NULL COMMENT 'セグメントコード', ADD UNIQUE KEY uq_segments_code (code);
ALTER TABLE units         MODIFY COLUMN code VARCHAR(50) NOT NULL COMMENT 'ユニットコード', ADD UNIQUE KEY uq_units_code (code);
