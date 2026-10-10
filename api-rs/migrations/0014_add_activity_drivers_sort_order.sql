-- ドライバーの表示順（数値入力画面のドラッグで並び替える）。既存のドライバーは作成順にする。

ALTER TABLE activity_drivers ADD COLUMN sort_order INT NOT NULL DEFAULT 0 COMMENT '施策内の表示順' AFTER unit;

UPDATE activity_drivers d
JOIN (SELECT id, ROW_NUMBER() OVER (PARTITION BY activity_id ORDER BY id) AS rn FROM activity_drivers) t ON t.id = d.id
SET d.sort_order = t.rn;
