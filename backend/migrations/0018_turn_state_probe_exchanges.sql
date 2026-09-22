-- Codex turn state 探测报头属于短期敏感观测，不并入模型请求和用量事实。
create table turn_state_probe_exchanges (
  id text primary key,
  trigger text not null,
  account_id text not null,
  model_id text not null,
  target_id text not null,
  target_label text not null,
  request_id text not null,
  started_at timestamptz not null,
  finished_at timestamptz not null,
  status_code integer,
  http_version text,
  outcome text not null,
  latency_ms bigint not null,
  request_headers_json jsonb not null,
  response_headers_json jsonb not null,
  request_header_count integer not null,
  response_header_count integer not null,
  request_header_bytes bigint not null,
  response_header_bytes bigint not null,
  request_turn_state_count integer not null,
  request_turn_state_length integer,
  request_turn_state_sha256 text,
  response_turn_state_count integer not null,
  response_turn_state_length integer,
  response_turn_state_sha256 text,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  constraint turn_state_probe_exchanges_trigger_ck check (
    trigger in ('manual_probe', 'automatic_renewal')
  ),
  constraint turn_state_probe_exchanges_status_ck check (
    status_code is null or status_code between 100 and 999
  ),
  constraint turn_state_probe_exchanges_counts_ck check (
    latency_ms >= 0
    and request_header_count >= 0
    and response_header_count >= 0
    and request_header_bytes >= 0
    and response_header_bytes >= 0
    and request_turn_state_count >= 0
    and response_turn_state_count >= 0
  ),
  constraint turn_state_probe_exchanges_headers_ck check (
    jsonb_typeof(request_headers_json) = 'array'
    and jsonb_typeof(response_headers_json) = 'array'
  ),
  constraint turn_state_probe_exchanges_state_ck check (
    (request_turn_state_length is null) = (request_turn_state_sha256 is null)
    and (response_turn_state_length is null) = (response_turn_state_sha256 is null)
    and (request_turn_state_count = 1) = (request_turn_state_length is not null)
    and (response_turn_state_count = 1) = (response_turn_state_length is not null)
    and coalesce(request_turn_state_length, 0) >= 0
    and coalesce(response_turn_state_length, 0) >= 0
  )
);

create index turn_state_probe_exchanges_created_idx
  on turn_state_probe_exchanges (created_at desc, id desc);

create index turn_state_probe_exchanges_subject_idx
  on turn_state_probe_exchanges (account_id, model_id, created_at desc, id desc);

create index turn_state_probe_exchanges_expiry_idx
  on turn_state_probe_exchanges (expires_at, id);
