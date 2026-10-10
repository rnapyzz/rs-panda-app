-- 画面の簡素化（docs/plan.md「2.18」「13」）。
--
-- 想定条件（scenario_conditions）を、今回の見込の説明（activity_scenario_notes.explanation）に統合する。
-- - 説明があれば末尾に「（想定条件）」に続けて追記し、なければ想定条件をそのまま説明にする
-- - 施策の更新の状態（last_edited_at・completed_at）は変えない
-- - 移行は1つの変更セット（操作した人は最初の FP&A）として変更履歴に残す
-- - scenario_conditions のテーブルとデータは残す

INSERT INTO change_sets (user_id, scenario_id, reason)
SELECT (SELECT MIN(id) FROM users WHERE role = 'fpa_admin'), NULL, '想定条件を今回の見込の説明に統合（データの移行）'
FROM DUAL
WHERE EXISTS (SELECT 1 FROM scenario_conditions WHERE description <> '')
  AND EXISTS (SELECT 1 FROM users WHERE role = 'fpa_admin');
SET @cs = IF(ROW_COUNT() > 0, LAST_INSERT_ID(), NULL);
SET @max_note = (SELECT COALESCE(MAX(id), 0) FROM activity_scenario_notes);

-- 説明がある施策 × シナリオ: 追記する
INSERT INTO audit_logs (change_set_id, table_name, record_id, action, before_json, after_json)
SELECT @cs, 'activity_scenario_notes', n.id, 'update',
       JSON_OBJECT('scenario_id', n.scenario_id, 'activity_id', n.activity_id, 'explanation', COALESCE(n.explanation, '')),
       JSON_OBJECT('scenario_id', n.scenario_id, 'activity_id', n.activity_id, 'explanation',
                   IF(COALESCE(n.explanation, '') = '', c.description, CONCAT(n.explanation, '\n\n（想定条件）', c.description)))
FROM activity_scenario_notes n
JOIN scenario_conditions c ON c.scenario_id = n.scenario_id AND c.activity_id = n.activity_id
WHERE @cs IS NOT NULL AND c.description <> '';

UPDATE activity_scenario_notes n
JOIN scenario_conditions c ON c.scenario_id = n.scenario_id AND c.activity_id = n.activity_id
SET n.explanation = IF(COALESCE(n.explanation, '') = '', c.description, CONCAT(n.explanation, '\n\n（想定条件）', c.description))
WHERE c.description <> '';

-- 説明がない施策 × シナリオ: 想定条件を説明にする（状態は未着手のまま）
INSERT INTO activity_scenario_notes (scenario_id, activity_id, explanation)
SELECT c.scenario_id, c.activity_id, c.description
FROM scenario_conditions c
WHERE c.description <> ''
  AND NOT EXISTS (SELECT 1 FROM activity_scenario_notes n WHERE n.scenario_id = c.scenario_id AND n.activity_id = c.activity_id);

INSERT INTO audit_logs (change_set_id, table_name, record_id, action, before_json, after_json)
SELECT @cs, 'activity_scenario_notes', n.id, 'insert', NULL,
       JSON_OBJECT('scenario_id', n.scenario_id, 'activity_id', n.activity_id, 'explanation', n.explanation)
FROM activity_scenario_notes n
WHERE @cs IS NOT NULL AND n.id > @max_note;
