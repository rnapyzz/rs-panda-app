-- マスタ変更では変更理由を任意とするため、change_sets.reason を NULL 可にする。
-- 金額・ドライバーの変更では引き続きアプリ側で理由を必須にする。

ALTER TABLE change_sets
    MODIFY COLUMN reason TEXT NULL COMMENT '変更理由（金額・ドライバーの変更では必須）';
