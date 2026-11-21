create table session (
  id text primary key,
  parent_id text,
  directory text not null,
  title text,
  time_created integer not null,
  time_updated integer not null
);
create table message (
  id text primary key,
  session_id text not null,
  time_created integer not null,
  data text not null
);
create table part (
  id text primary key,
  message_id text not null,
  session_id text not null,
  time_created integer not null,
  data text not null
);

insert into session values (
  'sess_zcode', null, '/tmp/zcode-project', 'ZCode interaction fixture', 1000, 3000
);
