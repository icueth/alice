# Codex rollout format (structure only)

Derived from one real Codex CLI rollout (`sample-1.jsonl`; `session_meta.payload.cli_version` `0.154.0`,
`originator: "codex-tui"`, `source: "cli"`). Only one rollout was sampled, so every statement below is "seen in
this rollout"; older or newer Codex versions may differ. **A parser must ignore unknown top-level `type` values,
unknown `payload.type` values and unknown fields.**

The fixture is scrubbed exactly like the Claude fixtures (see `../claude/FORMAT.md`): free text is `lorem-<n>`
filler, ids are rewritten consistently, timestamps are shifted by one fixed offset, `cwd` and paths are
`/home/user/project-x/...`, `encrypted_content` and image data are dummy base64. Kept verbatim: keys, JSON
types, `type` values, `role`, `phase`, `status`, tool names (`name`), enum-like strings, numbers and booleans.
The fixture is four windows of the source rollout (head, one `function_call` region, and the region around the
first two `compacted` lines), arrays cut to 12 items and objects to 40 keys, so some call/output pairs and
`turn_id` groups are cut at window edges.

## File layout

- `~/.codex/sessions/YYYY/MM/DD/rollout-<YYYY-MM-DD>T<HH-MM-SS>-<uuid>.jsonl`. The `<uuid>` equals
  `session_meta.payload.id` (true for all 40 rollouts checked) and the first line of every rollout checked (124)
  is `session_meta`.
- One JSON object per line, UTF-8, UTC timestamps `YYYY-MM-DDTHH:MM:SS.mmmZ`.

## Envelope

Every line is exactly `{"timestamp": string, "type": string, "ordinal": integer, "payload": object}`.
`ordinal` starts at 0 and increases by one per line. The line `type` selects the payload;
for `event_msg` and `response_item` the payload has its own string `type`; for the other line types the payload has
no `type` field.

## `type` / `payload` mapping

| line `type` | `payload.type` | payload fields (JSON types) | meaning |
|---|---|---|---|
| `session_meta` | none | `id` string, `session_id` string, `timestamp` string, `cwd` string, `originator` string, `cli_version` string, `source` string, `thread_source` string, `model_provider` string, `history_mode` string, `base_instructions {provenance {type, model}, text}`, `context_window {window_id}`, `git {branch, commit_hash, repository_url}?` | first line; session identity and working directory (`git` is optional) |
| `turn_context` | none | `turn_id`, `root_turn_id`, `cwd`, `workspace_roots[]`, `current_date`, `timezone`, `approval_policy`, `approvals_reviewer`, `sandbox_policy {...}`, `permission_profile {...}`, `active_permission_profile?`, `file_system_sandbox_policy?`, `model`, `effort`, `summary`, `personality`, `collaboration_mode {mode, settings {developer_instructions, model, reasoning_effort}}`, `multi_agent_version`, `comp_hash`, `realtime_active` boolean | settings in force for one turn; may repeat with a new `cwd` |
| `world_state` | none | `full` boolean, `state` object (`permissions`, optional `environments {current_date, timezone, filesystem, environments {local {cwd, shell, status}}}`, `agents_md {directory?, text}`, `skills`, `host_skills`, `collaboration_mode`, `personality`, `realtime`, ...) | snapshot of injected context; not conversation |
| `token_usage_record` | none | `thread_id`, `turn_id`, `session_id`, `root_turn_id`, `response_id` (strings), `usage`, `turn_token_usage`, `thread_token_usage` (objects of integers: `input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`, `reasoning_output_tokens`, `total_tokens`) | per-response token usage |
| `compacted` | none | `message` string, `replacement_history[]`, `guardian_history[]`, `retained_context {incomplete, next_order, user_messages[], user_messages_incomplete, verified_answers[]}`, `window_number` integer, `first_window_id`, `previous_window_id`, `window_id`, `compaction_response_id` (strings), `latest_token_usage_record` object | a compaction; see below |
| `response_item` | `message` | `id`, `role` (`user`, `assistant`, `developer`), `content[]`, `phase?` (assistant: `commentary` or `final_answer`) | model-facing conversation message |
| `response_item` | `reasoning` | `id`, `summary[]`, `encrypted_content` string | model reasoning (the text is encrypted) |
| `response_item` | `function_call` | `id`, `name`, `namespace?`, `arguments` string (JSON text), `call_id` | a function tool call (seen: `wait`, `clock`/`sleep`, `request_user_input_async`) |
| `response_item` | `function_call_output` | `id`, `call_id`, `output` string or array of `{type: "input_text", text}` | result of a function call |
| `response_item` | `custom_tool_call` | `id`, `name`, `input` string, `call_id`, `status` | a custom (freeform) tool call; all seen are `name: "exec"` with `status: "completed"` |
| `response_item` | `custom_tool_call_output` | `id`, `call_id`, `output` string or array of `input_text` / `input_image` blocks | result of a custom tool call |
| `event_msg` | `item_completed` | `thread_id`, `turn_id`, `item` object, `started_at_ms`, `completed_at_ms` | UI-level event: one finished item; see item types |
| `event_msg` | `task_started` | `turn_id`, `started_at`, `model_context_window`, `collaboration_mode_kind` | start of a turn |
| `event_msg` | `task_complete` | `turn_id`, `last_agent_message` string, `started_at`, `completed_at`, `duration_ms`, `time_to_first_token_ms` | end of a turn |
| `event_msg` | `token_count` | `info {last_token_usage, total_token_usage, model_context_window}`, `rate_limits {...}` | running token counters |
| `event_msg` | `thread_settings_applied` | `thread_id`, `thread_settings {...}` | settings change |

Every `response_item` payload also carries `internal_chat_message_metadata_passthrough {turn_id, create_time?,
content_item_kinds[]?}` (the key name is the one in the file); `turn_id` ties the item to the turn.

### Newer builds (not in the fixture)

A names-only scan of one person's rollouts of seven days (2026-10-18: 35 rollouts, 17,829 lines; type and field
names and counts, no values) found two shapes this table lacks, 40 lines each, in multi-agent sessions:

| line `type` | `payload.type` | payload fields | Alice |
|---|---|---|---|
| `inter_agent_communication_metadata` | none | `trigger_turn` | known, dropped (bookkeeping) |
| `response_item` | `agent_message` | `id`, `author`, `recipient`, `content[]` (`{type: "input_text", text}`, and in 30 of 40 also `{type: "encrypted_content", encrypted_content}`), `internal_chat_message_metadata_passthrough` | not read yet: counted as unrecognised |

`agent_message` carries text (a message between agents), so it is not bookkeeping; reading it needs a sampled
fixture first.

As for Claude Code, Alice drops a line of a top-level `type` it does not know as **bookkeeping**, counted apart
and never as drift, only when none of these keys appears in it at any depth, its `payload` and the arrays in it
included: `message`, `content`, `text`, `summary`, `arguments`, `input`, `output`, `prompt`, `command`, `result`, `error`, `thinking`, `reasoning` (the keys Codex keeps tool calls and outputs under, `arguments`, `input` and `output`,
among them). With such a key anywhere, or with a `payload` that is not an object, the line counts as unrecognised.
An unknown `payload.type` of a known line type (`response_item`, as `agent_message` is) always counts as
unrecognised.

### `response_item.message` content blocks

`content[]` blocks are `{type: "input_text", text}` for `user`/`developer` and `{type: "output_text", text}` for
`assistant`. A `user` message is either the typed prompt or an injected context message (the one seen starts with
an `<environment_context>` tag, and `internal_chat_message_metadata_passthrough.content_item_kinds` then lists
kinds such as `user.text`, `environments.environment_context`, `permissions.instructions`; typed text is
`user.text`). `developer` messages are harness instructions, not conversation.

### `event_msg.item_completed` item types

`item.type` is one of (fields in this rollout):

| `item.type` | fields |
|---|---|
| `UserMessage` | `id`, `client_id`, `content[{type, text, text_elements[]}]` (typed prompt; 9 here, versus 11 `user` messages in `response_item`, the rest being injected context) |
| `AgentMessage` | `id`, `phase`, `content[{type, text}]`, `delivery?`, `questions[]?` |
| `Reasoning` | `id`, `summary_text[]`, `raw_content[]` |
| `CommandExecution` | `id`, `command[]` (argv strings), `cwd`, `parsed_cmd[{type, cmd}]`, `process_id` string, `source`, `status`, `exit_code`, `duration {secs, nanos}`, `stdout`, `stderr`, `aggregated_output`, `formatted_output` |
| `FileChange` | `id`, `status`, `stdout`, `stderr`, `changes` (object keyed by file path; values `{type: "add" \| "update" \| "delete", content?, unified_diff?, move_path?}`) |
| `Extension` | `id`, `kind`, `durationMs` |
| `ImageView` | `id`, `path` |
| `ContextCompaction` | `id` |

`UserMessage` and `AgentMessage` events duplicate the corresponding `response_item.message` text and are the
simplest source for "what the human typed" and "what the assistant said". `AgentMessage.phase` is
`commentary` (progress notes) or `final_answer`.

## Tool calls and results

`call_id` pairs a `function_call` / `custom_tool_call` with its `*_output` line (822 of 822 calls had exactly one
output in this rollout). In the source `function_call.arguments` is JSON text (it parsed as JSON in all 209 cases); in this fixture the scrub replaced it with filler, so it no longer parses as JSON. The shell is
reached through the custom tool `exec` (its `input` is a string of code) and shows up again as
`CommandExecution` events; `output` of a custom-tool result is an array of `input_text` blocks (or one
`input_text` plus an `input_image` for screenshots), or a plain string.

## Compaction

A `compacted` line (4 in the full rollout, 2 in the fixture) stands alone between ordinary lines: the line
before was a `token_usage_record`, the line after a `world_state`, then a `turn_context`. Its
`replacement_history[]` holds `message` objects (roles `user` and `developer`) and one `{type: "compaction"}`
object; `guardian_history[]` is a long array (about 130 entries); `retained_context` carries the user messages
kept. An `event_msg.item_completed` with `item.type: "ContextCompaction"` accompanies it. Earlier lines stay in
the file, so a parser that wants each turn once should not re-read `replacement_history` as new conversation.

## Windows (what the fixture keeps of the source rollout)

| fixture lines | source lines |
|---|---|
| 1-60 | 1-60 |
| 61-71 | 330-340 |
| 72-87 | 565-580 (holds the first `compacted` line, fixture line 77) |
| 88-94 | 2254-2260 (starts with the second `compacted` line, fixture line 88) |

These are **artifacts of the windows, not features of the format**:

- `ordinal` jumps at fixture lines 61, 72 and 88 (the source values are kept). In a whole rollout it starts at 0
  and increases by one per line.
- The line before the `compacted` line at fixture line 88 is a window cut (source line 580), not a
  `token_usage_record`. The `compacted` line at fixture line 77 does have the `token_usage_record` before it, as
  in the source.
- Calls and outputs, and `turn_id` groups, can be cut at a window edge (a `call_id` can be unpaired).
- Arrays are cut to 12 items (so `guardian_history` and `replacement_history` are far shorter than in the source),
  strings to 300 characters.
- Timestamps and payload times are shifted into the past by one fixed offset, all before 2026-10-16.

## Not in the fixture

Seen in the source rollout but absent from the fixture windows: `event_msg.item_completed` items of type
`FileChange`, `Extension` and `ImageView`; `event_msg` `task_complete`; `custom_tool_call_output` with an
`input_image` block; `namespace` on `function_call` (the one fixture `function_call` has none); `compacted`
payload `replacement_history` entries other than the ones kept.

## Parser checklist

1. Check the envelope (`timestamp`, `type`, `ordinal`, `payload`); skip lines that do not parse. Tell an unknown
   line `type` with no conversation key at any depth (bookkeeping) from one with such a key anywhere.
2. Typed prompts: `event_msg` / `item_completed` / `UserMessage` (or `response_item.message` with `role: "user"` and
   `content_item_kinds` containing `user.text`). Assistant text: `AgentMessage` or `response_item.message` with
   `role: "assistant"`.
3. Ignore `developer` messages, `world_state`, `turn_context`, `token_*` and `reasoning` for text capture.
4. Pair tool calls by `call_id`.
5. Take the working directory from `session_meta.payload.cwd`, refreshed by each `turn_context.payload.cwd`.
