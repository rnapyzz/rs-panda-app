-- 「機能」を「ユニット」に改称したのに合わせて、内部の名前も functions → units にそろえる。
-- データはそのまま引き継ぐ。過去の監査ログ（audit_logs）の table_name / JSON は記録どおり残す
-- （functions / function_id のまま。変更履歴の表示ではユニットとして扱う）。

-- 外部キーを外してから名前を変え、新しい名前で付け直す
ALTER TABLE activities DROP FOREIGN KEY fk_activities_function;
ALTER TABLE functions
    DROP FOREIGN KEY fk_functions_segment,
    DROP FOREIGN KEY fk_functions_organization,
    DROP FOREIGN KEY fk_functions_owner;

RENAME TABLE functions TO units;

ALTER TABLE units
    RENAME INDEX idx_functions_segment TO idx_units_segment,
    RENAME INDEX idx_functions_organization TO idx_units_organization,
    RENAME INDEX idx_functions_owner TO idx_units_owner,
    ADD CONSTRAINT fk_units_segment      FOREIGN KEY (segment_id)      REFERENCES segments (id),
    ADD CONSTRAINT fk_units_organization FOREIGN KEY (organization_id) REFERENCES organizations (id),
    ADD CONSTRAINT fk_units_owner        FOREIGN KEY (owner_user_id)   REFERENCES users (id),
    COMMENT = 'ユニットマスタ（施策を束ねる単位）';

ALTER TABLE activities
    RENAME COLUMN function_id TO unit_id,
    RENAME INDEX idx_activities_function TO idx_activities_unit,
    ADD CONSTRAINT fk_activities_unit FOREIGN KEY (unit_id) REFERENCES units (id);
