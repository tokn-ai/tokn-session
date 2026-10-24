-- Representative tables/projections from OpenCode 1.18.8,
-- 017a5977d2107092007623e507fc5c6eb337d3b2 packages/core/src/session/sql.ts.
create table session (
  id text primary key, parent_id text, directory text not null, title text,
  model text, time_created integer not null, time_updated integer not null
);
create table message (id text primary key, session_id text, time_created integer, data text);
create table part (id text primary key, message_id text, session_id text, time_created integer, data text);
create table session_message (
  id text primary key, session_id text not null, type text not null, seq integer not null,
  time_created integer not null, time_updated integer not null, data text not null
);
insert into session values ('ses_empty', null, '/work/project', 'New session - 2026-10-25T00:00:00.000Z', null, 100, 100);
insert into session values ('ses_v2', null, '/work/project', 'New session - 2026-10-25T00:00:00.000Z', '{"providerID":"local","id":"latest"}', 100, 120);
insert into session values ('ses_v1', null, '/work/project', 'Legacy session', null, 100, 120);
insert into message values ('old_user', 'ses_v1', 101, '{"role":"user"}');
insert into part values ('old_text', 'old_user', 'ses_v1', 101, '{"type":"text","text":"legacy prompt"}');
insert into session_message values ('msg_user', 'ses_v2', 'user', 0, 110, 110,
  '{"text":"Inspect this image","files":[{"uri":"data:image/png;base64,aGVsbG8=","mime":"image/png","name":"image.png"}],"time":{"created":110}}');
insert into session_message values ('msg_answer', 'ses_v2', 'assistant', 1, 105, 120,
  '{"agent":"build","model":{"providerID":"local","id":"first"},"content":[{"type":"reasoning","id":"reason_1","text":"Thinking"},{"type":"tool","id":"call_1","name":"question","state":{"status":"completed","input":{"questions":[{"question":"Which?","header":"Choice","options":[{"label":"A","description":"First"}]}]},"structured":{},"content":[{"type":"text","text":"A"}],"attachments":[{"uri":"data:image/png;base64,aGVsbG8=","mime":"image/png"}]},"time":{"created":106,"completed":108}},{"type":"text","id":"text_1","text":"Answer"}],"finish":"stop","tokens":{"input":10,"output":4,"reasoning":2,"cache":{"read":3,"write":1}},"time":{"created":105,"completed":120}}');
insert into session_message values ('msg_model', 'ses_v2', 'model-switched', 2, 121, 121,
  '{"model":{"providerID":"local","id":"latest"},"time":{"created":121}}');
