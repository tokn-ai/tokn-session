-- Representative current storage from 5756d59a68a3d16ce412280c3ab055031898cce2,
-- packages/core/src/session/sql.ts and packages/schema/src/session-message.ts.
-- Blob bytes are deliberately minimal test data, not a native capture.
create table session_v2 (
  id text primary key,parent_id text,directory text not null,title text,model text,
  time_created integer not null,time_updated integer not null
);
create table session_message (
  id text primary key,session_id text not null,type text not null,seq integer not null,
  time_created integer not null,time_updated integer not null,data text not null
);
create table session_part (
  message_id text not null,session_id text not null,ordinal integer not null,type text not null,
  call_id text,time_created integer not null,time_updated integer not null,data text not null,
  primary key(message_id,ordinal)
);
create table blob (
  hash text primary key,mime text not null,bytes integer not null,width integer,height integer,
  data blob not null,time_created integer not null
);
create table blob_ref (
  session_id text not null,message_id text not null,hash text not null,seq integer not null,
  primary key(session_id,message_id,hash)
);
insert into session_v2 values ('ses_current',null,'/work/current',null,'{"providerID":"fixture","id":"model"}',100,200);
insert into session_v2 values ('ses_current_empty',null,'/work/current','Empty',null,100,100);
insert into session_message values ('msg_user','ses_current','user',0,110,110,
  '{"text":"Current prompt","time":{"created":110},"files":[{"uri":"media:ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb","mime":"image/png","source":{"type":"inline"}}]}');
insert into session_message values ('msg_assistant','ses_current','assistant',1,120,150,
  '{"agent":"build","model":{"providerID":"fixture","id":"model"},"time":{"created":120,"completed":150},"content":[{"type":"text","text":" inline tail"}],"tokens":{"input":10,"output":4,"reasoning":2,"cache":{"read":3,"write":1}}}');
insert into session_part values ('msg_assistant','ses_current',0,'reasoning',null,120,120,
  '{"type":"reasoning","text":"Thinking","native":{"signature":"fixture-signature"}}');
insert into session_part values ('msg_assistant','ses_current',1,'text',null,121,121,
  '{"type":"text","text":"First"}');
insert into session_part values ('msg_assistant','ses_current',2,'tool','call_stream',122,122,
  '{"type":"tool","id":"call_stream","name":"bash","state":{"status":"streaming","input":"{\"command\":"},"time":{"created":122}}');
insert into session_part values ('msg_assistant','ses_current',3,'tool','call_done',123,130,
  '{"type":"tool","id":"call_done","name":"bash","state":{"status":"completed","input":{"command":"printf fixture"},"metadata":{"exit":0,"fixture":true},"content":[{"type":"text","text":"fixture"},{"type":"file","mime":"image/png","uri":"media:ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"}]},"time":{"created":123,"ran":124,"completed":130}}');
insert into session_part values ('msg_assistant','ses_current',4,'future',null,125,125,
  '{"type":"future","opaque":{"preserved":true}}');
insert into session_message values ('msg_shell','ses_current','shell',2,160,170,
  '{"shellID":"sh_fixture","command":"printf shell","status":"exited","exit":0,"output":{"output":"shell","cursor":5,"size":5,"truncated":false},"time":{"created":160,"completed":170}}');
insert into session_message values ('msg_compaction_running','ses_current','compaction',3,171,171,
  '{"status":"running","reason":"auto","summary":"working","time":{"created":171}}');
insert into session_message values ('msg_compaction_done','ses_current','compaction',4,172,172,
  '{"status":"completed","reason":"auto","summary":"summary","metadata":{"keepID":"msg_user"},"time":{"created":172}}');
insert into session_message values ('msg_compaction_failed','ses_current','compaction',5,173,173,
  '{"status":"failed","reason":"manual","error":{"name":"UnknownError","data":{"message":"fixture failure"}},"time":{"created":173}}');
insert into session_message values ('msg_skill','ses_current','skill',6,174,174,
  '{"skill":"fixture-skill","name":"Fixture","text":"Fixture skill text","time":{"created":174}}');
insert into session_message values ('msg_location','ses_current','location-switched',7,175,175,
  '{"location":{"id":"fixture-location"},"projectID":"fixture-project","subpath":"nested","time":{"created":175}}');
insert into session_message values ('msg_idle','ses_current','idle',8,176,176,
  '{"outcome":"succeeded","time":{"created":176}}');
insert into blob values ('ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb','image/png',1,1,1,x'61',110);
insert into blob_ref values ('ses_current','msg_user','ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb',0);
insert into blob_ref values ('ses_current','msg_assistant','ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb',1);
