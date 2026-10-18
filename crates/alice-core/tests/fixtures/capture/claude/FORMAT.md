# Claude Code transcript format (structure only)

Derived from two real Claude Code sessions (`sample-1.jsonl`, `sample-2.jsonl`) and one subagent transcript
(`sample-1-agent-1.jsonl`). Claude Code versions seen: 2.1.296 and 2.1.300. The format is undocumented and
changes between versions: **a parser must ignore line types and fields it does not know, and must treat every
field except `type` as optional.**

The fixtures are scrubbed, so they contain no original text. Every free-text string is `lorem-<n> ...` filler of
similar length (capped at 300 characters); ids are rewritten consistently (same input, same output); timestamps
are shifted by one fixed offset (order and ISO format kept); `cwd` and paths are `/home/user/project-x/...`;
base64 data is a tiny dummy string. Kept verbatim: keys, JSON types, `type`/`subtype`/`role`, tool names, enum-like
values (`stop_reason`, `level`, `userType`, `entrypoint`, `permissionMode`, ...), booleans and numbers. Harness
wrapper tags at the start of a text value (for example `<system-reminder>`, `<task-notification>`,
`<command-name>`) are kept as an opening tag, filler, and a closing tag. Arrays are cut to 12 items and objects
to 40 keys, and each fixture is a few windows of the source session (head, around a tool error or image, and
around the compaction), so some `tool_use`/`tool_result` pairs and `parentUuid` links are cut at window edges.

## Files and layout

- Main transcript: `~/.claude/projects/<project-dir>/<sessionId>.jsonl`. `<project-dir>` is the session `cwd` with
  every non-alphanumeric character replaced by `-` (true for all 60 files checked). The file stem equals the
  `sessionId` field. One session file holds one `sessionId` (none of the files checked had two).
- One JSON object per line, UTF-8. Line order is write order; it is not strictly timestamp order, and
  bookkeeping lines are interleaved with the conversation.
- Subagent transcript: `~/.claude/projects/<project-dir>/<sessionId>/subagents/agent-<agentId>.jsonl`, plus
  `agent-<agentId>.meta.json` (one JSON object: `agentType` string, `description` string, `toolUseId` string,
  `spawnDepth` integer, `requestShape` string, `requestNonInteractive` boolean, `model` string).
  The `<sessionId>/` directory also had a `tool-results/` directory (not sampled).
- A subagent file uses the same line types as a main file. Differences: every `user`/`assistant`/`attachment`
  line carries `agentId` (string, equal to the `<agentId>` in the file name), `isSidechain` is `true`, `sessionId`
  is the parent session's id, and the first line has `parentUuid: null`. Assistant lines may add
  `attributionAgent`. The parent links to it from the `user` line that holds the `Agent` tool's result:
  `toolUseResult.agentId` equals the subagent's `agentId`, and `meta.json.toolUseId` names the parent's `tool_use`
  block. In the main file `isSidechain` was `false` on every line checked (the field is present on `user`,
  `assistant`, `attachment` and most `system` lines).

## Line types

Every line is an object with a string `type`. `system` lines also have a string `subtype`. Types seen in the
full sessions (the fixtures hold windows of them):

| `type` / `subtype` | role in the file | has `uuid` / `parentUuid` / `timestamp` |
|---|---|---|
| `user` | a human prompt, a tool result, an injected meta message, or the compaction summary | yes / yes / yes |
| `assistant` | one content block of a model response (see "message.id splits") | yes / yes / yes |
| `attachment` | harness-injected context (hooks, environment, file reads, tool lists, reminders) | yes / yes / yes |
| `system/turn_duration` | end-of-turn timing, `isMeta: false` in all seen | yes / yes / yes |
| `system/stop_hook_summary` | result of Stop hooks | yes / yes / yes |
| `system/compact_boundary` | marks a compaction | yes / `null` (plus `logicalParentUuid`) / yes |
| `system/informational` | notice text, `isMeta: false` in all seen | yes / yes / yes |
| `queue-operation` | prompt queue event (`enqueue`, `dequeue`, `remove`) | no / no / yes |
| `file-history-snapshot` | file backup index | no / no / only inside `snapshot` |
| `file-history-delta` | one tracked-file backup | no / no / yes |
| `last-prompt`, `mode`, `permission-mode`, `ai-title`, `atis-latch` | per-session bookkeeping, re-appended many times (about once per turn); each carries `sessionId` and one value field | no / no / no |
| `api-request-shape`, `api-request`, `api-request-blob` | raw API request logging (present in these builds; large) | no / no / partly |

**Dispatch on the line `type` first.** `api-request-blob` lines carry a `message` object with `role` `user`,
`assistant` or `system` and `tool_use`, `tool_result`, `thinking` and `text` blocks, i.e. they look like
conversation messages. They are request-log copies. A parser that keys on `message.role` or scans for content
blocks without checking `type` will double count (or count system prompts as turns).

The bookkeeping and `api-request*` lines are not conversation. A capture parser should read only `user`,
`assistant`, `system/compact_boundary` and, optionally, `attachment`, and skip everything else without failing.
`leafUuid` on `last-prompt` points at the `uuid` of the latest conversation line.

### Bookkeeping types of newer builds (not in the fixtures)

A names-only scan of one person's transcripts of seven days (2026-10-18; type and field names and counts, no
values) found eight more top-level types. None carries conversation, and none is in the fixtures. Their fields:

| `type` | fields | count |
|---|---|---|
| `frame-link` | `sessionId`, `path?`, `frameUrl?`, `title?`, `artifactCount`, `timestamp` | 2735 |
| `artifact-autoreact-ledger` | `v`, `sessionId`, `accountUuid`, `artifacts {<id>: {savedAt, stampHighWater, everBaselined, everHadThreads, turnTimestamps, threads}}` | 556 |
| `bridge-session` | `sessionId`, `bridgeSessionId`, `lastSequenceNum`, `ownerAccountUuid?`, `ownerOrganizationUuid?`, `noHistoryBackfill?` | 180 |
| `artifact-comment-monitor` | `v`, `sessionId`, `artifacts {<id>: {state, writtenAtMs, title}}` | 56 |
| `cost-state` | `sessionId`, `totalCostUSD`, `totalAPIDuration`, `totalAPIDurationWithoutRetries`, `totalDuration`, `totalToolDuration`, `totalLinesAdded`, `totalLinesRemoved`, `startTime`, `hasUnknownModelCost`, `modelUsage {<model>: {inputTokens, ...}}` | 46 |
| `history-suppression` | `sessionId`, `cause`, `vetoedAgainstAccountUuid?`, `ts` | 8 |
| `custom-title` | `customTitle`, `sessionId` | 5 |
| `agent-name` | `agentName`, `sessionId` | 5 |

No unknown content-block type was found. Claude Code adds lines like these with most releases, so Alice's reader
drops a line of a `type` it does not know as **bookkeeping** only when none of the keys conversation, tool calls and
their results are kept under appears in it at any depth, in nested objects and inside arrays too: `message`, `content`, `text`, `summary`, `arguments`, `input`, `output`, `prompt`, `command`, `result`, `error`, `thinking`, `reasoning`.
Bookkeeping lines are counted apart (`alice doctor` may name them as a detail, never as a problem). A line of an
unknown `type` that has any of those keys anywhere is counted as unrecognised: the format may have changed, and
`alice doctor` warns. A line without a string `type` is unrecognised too.

## Conversation chain

`parentUuid` links a line to the previous conversation line. A `tool_result` `user` line has
`parentUuid == sourceToolAssistantUUID` == the `uuid` of the assistant line holding the matching `tool_use`
(checked for all 176 and 204 results in the two sessions). Blocks of one response normally chain to each other (the next block's `parentUuid` is the previous block's `uuid`:
203 of 224 consecutive pairs in one session, 227 of 245 in the other). When tool results or attachments are
written between two blocks of the same response (parallel tool calls), the later block's `parentUuid` is a
different line, so do not assume one linear chain; rebuild order from file order and use `parentUuid` only for
tool-result pairing and compaction handling.

## Fields by line type (from the fixtures)

`?` marks a field that is not always present. The tables are generated from the three Claude fixtures together.

#### `last-prompt` (20 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `lastPrompt` | string | sometimes |
| `leafUuid` | string | always |
| `sessionId` | string | always |
| `type` | string | always |

#### `mode` (21 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `mode` | string | always |
| `sessionId` | string | always |
| `type` | string | always |

#### `permission-mode` (19 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `permissionMode` | string | always |
| `sessionId` | string | always |
| `type` | string | always |

#### `atis-latch` (20 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `atis` | string | always |
| `sessionId` | string | always |
| `type` | string | always |

#### `attachment` (137 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `agentId` | string | sometimes |
| `attachment` | object | always |
| `cwd` | string | always |
| `entrypoint` | string | always |
| `gitBranch` | string | always |
| `isSidechain` | boolean | always |
| `parentUuid` | null or string | always |
| `rendered` | array | sometimes |
| `renderedBesideToolResult` | boolean | sometimes |
| `renderedRole` | string | sometimes |
| `sessionId` | string | always |
| `session_id` | string | sometimes |
| `slug` | string | sometimes |
| `timestamp` | string | always |
| `type` | string | always |
| `userType` | string | always |
| `uuid` | string | always |
| `version` | string | always |

#### `file-history-snapshot` (4 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `isSnapshotUpdate` | boolean | always |
| `messageId` | string | always |
| `snapshot` | object | always |
| `type` | string | always |

#### `user` (43 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `agentId` | string | sometimes |
| `cwd` | string | always |
| `entrypoint` | string | always |
| `gitBranch` | string | always |
| `isCompactSummary` | boolean | sometimes |
| `isMeta` | boolean | sometimes |
| `isSidechain` | boolean | always |
| `isVisibleInTranscriptOnly` | boolean | sometimes |
| `message` | object | always |
| `origin` | object | sometimes |
| `parentUuid` | null or string | always |
| `permissionDecision` | object | sometimes |
| `permissionMode` | string | sometimes |
| `promptId` | string | always |
| `promptSource` | string | sometimes |
| `queueSkipAttachments` | boolean | sometimes |
| `sessionId` | string | always |
| `session_id` | string | sometimes |
| `slug` | string | sometimes |
| `sourceToolAssistantUUID` | string | sometimes |
| `sourceToolUseID` | string | sometimes |
| `timestamp` | string | always |
| `toolUseResult` | array or object or string | sometimes |
| `turnCompanion` | boolean | sometimes |
| `turnOrigin` | string | sometimes |
| `turnPosition` | object | sometimes |
| `type` | string | always |
| `userType` | string | always |
| `uuid` | string | always |
| `version` | string | always |

#### `ai-title` (18 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `aiTitle` | string | always |
| `sessionId` | string | always |
| `type` | string | always |

#### `assistant` (56 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `advisorModel` | string | always |
| `agentId` | string | sometimes |
| `apiBlockIndex` | integer | always |
| `attributionAgent` | string | sometimes |
| `attributionMcpServer` | string | sometimes |
| `attributionMcpTool` | string | sometimes |
| `attributionPlugin` | string | sometimes |
| `attributionSkill` | string | sometimes |
| `cwd` | string | always |
| `effort` | string | always |
| `entrypoint` | string | always |
| `gitBranch` | string | always |
| `isSidechain` | boolean | always |
| `message` | object | always |
| `parentUuid` | string | always |
| `perTurnEffort` | string | always |
| `requestId` | string | always |
| `requestRef` | string | sometimes |
| `requestedModel` | string | always |
| `sessionId` | string | always |
| `session_id` | string | sometimes |
| `thinkingDisplay` | string | sometimes |
| `thinkingDurationMs` | integer | sometimes |
| `timestamp` | string | always |
| `type` | string | always |
| `userType` | string | always |
| `uuid` | string | always |
| `version` | string | always |
| `wireIngestContext` | object | sometimes |
| `wireToolInputs` | object | sometimes |

#### `api-request-shape` (2 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `agentId` | string | sometimes |
| `sessionId` | string | always |
| `shape` | object | always |
| `shapeHash` | string | always |
| `timestamp` | string | always |
| `type` | string | always |
| `version` | string | always |

#### `api-request-blob` (31 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `agentId` | string | sometimes |
| `hash` | string | always |
| `message` | object | always |
| `sessionId` | string | always |
| `type` | string | always |

#### `api-request` (18 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `agentId` | string | sometimes |
| `base` | string | sometimes |
| `cacheControl` | array | always |
| `id` | string | always |
| `keep` | integer | always |
| `params` | object | always |
| `querySource` | string | always |
| `requestDigest` | string | always |
| `sessionId` | string | always |
| `shapeHash` | string | always |
| `tail` | array | always |
| `timestamp` | string | always |
| `type` | string | always |
| `version` | string | always |

#### `system/stop_hook_summary` (5 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `cwd` | string | always |
| `entrypoint` | string | always |
| `gitBranch` | string | always |
| `hasOutput` | boolean | always |
| `hookAdditionalContext` | array | always |
| `hookCount` | integer | always |
| `hookErrors` | array | always |
| `hookInfos` | array | always |
| `isSidechain` | boolean | always |
| `level` | string | always |
| `parentUuid` | string | always |
| `preventedContinuation` | boolean | always |
| `sessionId` | string | always |
| `session_id` | string | always |
| `stopReason` | string | always |
| `subtype` | string | always |
| `timestamp` | string | always |
| `toolUseID` | string | always |
| `type` | string | always |
| `userType` | string | always |
| `uuid` | string | always |
| `version` | string | always |

#### `system/turn_duration` (5 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `cwd` | string | always |
| `durationMs` | integer | always |
| `entrypoint` | string | always |
| `gitBranch` | string | always |
| `isMeta` | boolean | always |
| `isSidechain` | boolean | always |
| `messageCount` | integer | always |
| `parentUuid` | string | always |
| `sessionId` | string | always |
| `subtype` | string | always |
| `timestamp` | string | always |
| `type` | string | always |
| `userType` | string | always |
| `uuid` | string | always |
| `version` | string | always |

#### `queue-operation` (4 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `content` | string | sometimes |
| `operation` | string | always |
| `sessionId` | string | always |
| `timestamp` | string | always |
| `type` | string | always |

#### `system/compact_boundary` (2 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `compactMetadata` | object | always |
| `content` | string | always |
| `cwd` | string | always |
| `entrypoint` | string | always |
| `gitBranch` | string | always |
| `isMeta` | boolean | always |
| `isSidechain` | boolean | always |
| `level` | string | always |
| `logicalParentUuid` | string | always |
| `parentUuid` | null | always |
| `sessionId` | string | always |
| `slug` | string | always |
| `subtype` | string | always |
| `timestamp` | string | always |
| `type` | string | always |
| `userType` | string | always |
| `uuid` | string | always |
| `version` | string | always |

#### `system/informational` (1 in fixtures)

| field | JSON type | presence |
|---|---|---|
| `content` | string | always |
| `cwd` | string | always |
| `entrypoint` | string | always |
| `gitBranch` | string | always |
| `isMeta` | boolean | always |
| `isSidechain` | boolean | always |
| `level` | string | always |
| `parentUuid` | string | always |
| `sessionId` | string | always |
| `slug` | string | always |
| `subtype` | string | always |
| `timestamp` | string | always |
| `type` | string | always |
| `userType` | string | always |
| `uuid` | string | always |
| `version` | string | always |


Nested objects (field: JSON type, `?` = not always present):

```
assistant.message: `container`: null, `content`: array, `context_management`: null, `diagnostics`: null, `id`: string, `input_transformations`: array, `model`: string, `role`: string, `stop_details`: null, `stop_reason`: null/string, `stop_sequence`: null, `type`: string, `usage`: object
assistant.message.usage: `cache_creation`: object, `cache_creation_input_tokens`: integer, `cache_read_input_tokens`: integer, `fallback_credit`: null?, `inference_geo`: string, `input_tokens`: integer, `iterations`: array?, `output_tokens`: integer, `output_tokens_details`: object?, `server_tool_use`: object?, `service_tier`: string, `speed`: string?
assistant block thinking : `signature`: string, `thinking`: string, `type`: string
assistant block tool_use : `caller`: object, `id`: string, `input`: object, `name`: string, `type`: string
assistant block text : `text`: string, `type`: string
tool_use.input for Agent : `description`: string, `model`: string, `prompt`: string, `subagent_type`: string
tool_use.input for Bash : `command`: string, `description`: string
tool_use.input for Read : `file_path`: string, `limit`: integer, `offset`: integer
tool_use.input for Skill : `skill`: string
tool_use.input for ToolSearch : `max_results`: integer, `query`: string
tool_use.input for mcp__server-a__page_eval : `expression`: string, `screenshot`: boolean?, `timeoutMs`: integer?
tool_use.input for mcp__plugin_mempalace_mempalace__mempalace_diary_write : `agent_name`: string, `entry`: string, `topic`: string, `wing`: string
tool_use.input for mcp__plugin_mempalace_mempalace__mempalace_search : `limit`: integer, `query`: string
tool_use.input for mcp__plugin_mempalace_mempalace__mempalace_status : (no fields)
user.message: `content`: array/string, `role`: string
user block tool_result : `content`: array/string, `is_error`: boolean?, `tool_use_id`: string, `type`: string
user block text : `text`: string, `type`: string
tool_result.content[] block tool_reference : `tool_name`: string, `type`: string
tool_result.content[] block text : `text`: string, `type`: string
tool_result.content[] block image : `source`: object, `type`: string
image.source: `data`: string, `media_type`: string, `type`: string
toolUseResult object shapes: 5
  toolUseResult{ `matches`: array, `query`: string, `total_deferred_tools`: integer } x 2
  toolUseResult{ `interrupted`: boolean, `isImage`: boolean, `noOutputExpected`: boolean, `stderr`: string, `stdout`: string } x 13
  toolUseResult{ `commandName`: string, `success`: boolean } x 2
  toolUseResult{ `agentId`: string, `canContinueAgent`: boolean, `canReadOutputFile`: boolean, `description`: string, `isAsync`: boolean, `outputFile`: string, `prompt`: string, `resolvedModel`: string, `status`: string } x 1
  toolUseResult{ `file`: object, `type`: string } x 1
toolUseResult when not an object: array (7), string (4)
attachment agent_listing_delta : `addedLines`: array, `addedTypes`: array, `builtInTypes`: array, `isInitial`: boolean, `removedTypes`: array, `showConcurrencyNote`: boolean, `type`: string
attachment auto_mode : `autoModeConsentFlow`: boolean, `bashFirst`: boolean, `bashFirstSteer`: string, `bypass`: boolean, `steerOnly`: boolean, `type`: string
attachment command_permissions : `allowedTools`: array, `type`: string
attachment compact_file_reference : `displayPath`: string, `filename`: string, `type`: string
attachment credential_org : `organizationUuid`: string, `type`: string
attachment date : `date`: string, `type`: string
attachment deferred_tools_delta : `addedLines`: array, `addedNames`: array, `failedMcpServers`: array?, `pendingMcpServers`: array?, `readdedNames`: array, `removedNames`: array, `surfacedDefinitions`: array?, `surfacedNames`: array?, `type`: string, `wireHiddenNames`: array
attachment deferred_tools_record : `entries`: array, `toolInputCopies`: array?, `type`: string
attachment environment : `changes`: array?, `snapshot`: object, `type`: string
attachment file : `content`: object, `displayPath`: string, `filename`: string, `type`: string
attachment hook_additional_context : `content`: array, `hookEvent`: string, `hookName`: string, `toolUseID`: string, `type`: string
attachment hook_success : `command`: string, `content`: string, `durationMs`: integer, `exitCode`: integer, `hookEvent`: string, `hookName`: string, `stderr`: string, `stdout`: string, `toolUseID`: string, `type`: string
attachment instructions : `files`: array, `type`: string
attachment invoked_skills : `skills`: array, `type`: string
attachment mcp_instructions_delta : `addedBlocks`: array, `addedNames`: array, `removedNames`: array, `type`: string
attachment model : `identity`: object, `text`: string, `type`: string
attachment nested_memory : `content`: object, `displayPath`: string, `path`: string, `type`: string
attachment prompt_snapshot : `cliPrefix`: string?, `contextRendering`: string, `echoWireToolInputs`: boolean, `inlineTools`: boolean?, `keptReminders`: boolean?, `reminderFold`: boolean, `systemPrompt`: array, `systemTurns`: boolean?, `toolChangeHeader`: boolean?, `tools`: array?, `type`: string
attachment remote_session_change : `commit`: string, `managedCommit`: boolean, `managedPr`: boolean, `pr`: string, `sendUserFileHint`: boolean, `type`: string, `url`: null
attachment session_context : `context`: object, `type`: string
attachment silent_turn_reminder : `text`: string, `type`: string
attachment skill_listing : `content`: string, `isInitial`: boolean, `names`: array, `skillCount`: integer, `type`: string
attachment total_tokens_reminder : `text`: string, `type`: string
compactMetadata: `cumulativeDroppedTokens`: integer, `durationMs`: integer, `postTokens`: integer, `preCompactDiscoveredTools`: array, `preTokens`: integer, `preservedMessages`: object, `preservedSegment`: object, `trigger`: string
  preservedSegment: `anchorUuid`: string, `headUuid`: string, `tailUuid`: string
  preservedMessages: `allUuids`: array, `anchorUuid`: string, `uuids`: array
file-history-snapshot .snapshot: `messageId`: string, `timestamp`: string, `trackedFileBackups`: object
file-history-delta .backup: (no such line in the fixtures; fields are listed under "Other line types")
api-request .params: `betas`: array, `context_management`: object, `max_tokens`: integer, `model`: string, `output_config`: object, `thinking`: object
api-request-blob .message: `content`: array, `output_config`: object?, `role`: string
api-request-shape .shape: `system`: array, `tools`: array
user.origin: `kind`: string, `producer`: string?, `runId`: string? | turnPosition: `promptIndex`: integer, `turnIndex`: integer | permissionDecision: `decision`: string, `reasonType`: string, `source`: string
```

## `user` lines

`message` is `{role: "user", content}` where `content` is a string or an array of blocks.

| shape | what it is | marks |
|---|---|---|
| `content` is a string, no `isMeta`, `origin.kind: "human"`, `promptSource: "typed"`, `turnOrigin: "human"` | a human prompt | `promptId`, `permissionMode`, `turnPosition {promptIndex, turnIndex}` |
| `content` is a string, `origin.kind: "task-notification"`, `promptSource: "system"`, `turnOrigin: "task_notification"` | a background-task notification written as a user turn (starts with a `<task-notification>` tag) | `origin.producer`, `origin.runId` |
| `content` is an array with one `tool_result` block | a tool result | `sourceToolAssistantUUID`, `toolUseResult`, `permissionDecision` |
| `isMeta: true`, `content` is an array of `text` blocks | injected harness text, not typed by the human (seen: the body of a skill loaded by the `Skill` tool, where `sourceToolUseID` names that `tool_use`; and a `<command-message>` text) | `sourceToolUseID`, `turnCompanion` |
| `isCompactSummary: true`, `content` is a string | the compaction summary (see below) | `isVisibleInTranscriptOnly: true` |

A human prompt is exactly: `type == "user"`, `isMeta` absent or false, `isCompactSummary` absent, `isSidechain`
false, `content` a string, and no `toolUseResult`. A string `content` can still start with a harness tag
(`<command-name>`, `<local-command-stdout>`, `<task-notification>`, ...), so a parser that wants only text the
human typed must also drop strings that start with those tags.

### `tool_result` blocks

A `tool_result` block is `{type, tool_use_id, content, is_error?}`; one block per `user` line.

- `tool_use_id` equals the `id` of a `tool_use` block in an earlier `assistant` line (`toolu_...` shape).
- `content` is a string (shell, edit, read results) or an array of blocks. Array element types seen: `text`
  (`{type, text}`), `image` (`{type, source: {type: "base64", media_type, data}}`), and `tool_reference`
  (`{type, tool_name}`, returned by `ToolSearch`). A result with an image is an array holding `image` and `text`.
- `is_error` is a boolean and is optional: `true` on failures, `false` on many successes, absent on others
  (always absent when `content` is an array). In every failing case seen (8 of 8) `content` was a string and
  the `user` line's `toolUseResult` was a string too.
- The `user` line's `toolUseResult` duplicates the result in structured form and varies by tool:
  - Bash: object `{stdout, stderr, interrupted, isImage, noOutputExpected}` (sometimes `returnCodeInterpretation`,
    `backgroundTaskId`, `bashEditDiff {changedFiles[], files[{filePath, hunks[{lines[], newLines, newStart, oldLines,
    oldStart}]}], moreFiles}`).
  - Read: `{type: "text", file: {filePath, content, numLines, startLine, totalLines}}`.
  - Write: `{type: "create", filePath, content, structuredPatch[], originalFile: null, ...}`.
  - Edit: `{filePath, oldString, newString, originalFile, structuredPatch[], userModified, replaceAll}`.
  - ToolSearch: `{matches[], query, total_deferred_tools}`.
  - Skill: `{success, commandName}`.
  - Agent (background launch): `{status: "async_launched", agentId, description, prompt, outputFile, isAsync,
    canReadOutputFile, canContinueAgent, resolvedModel}`.
  - MCP tools: an array of `{type: "text", text}` blocks.
  - failures: a string.
  A parser should read the `tool_result` block, not `toolUseResult`, and treat `toolUseResult` as optional.

## `assistant` lines and `message.id` splits

`message` mirrors a Messages API response: `{id, type: "message", role: "assistant", model, content[], stop_reason,
stop_sequence, stop_details, container, context_management, diagnostics, input_transformations[], usage}`.

- **One API response is written as several lines.** Each `assistant` line holds exactly one block in
  `message.content` (a `thinking`, `text` or `tool_use` block). All lines of one response share `message.id`
  (`msg_...`) and `requestId` (`req_...`); `apiBlockIndex` counts 0, 1, 2 ... within the response; `timestamp` and
  `uuid` differ per line. In the main sessions every line of a response repeated the same `usage` and
  `stop_reason` (129 of 129 and 154 of 154 multi-line responses), so **do not sum `usage` across lines of one
  `message.id`**: count each `message.id` once. In the subagent file earlier lines of a response can have
  `stop_reason: null` and a different `usage` (only 17 of 51 multi-line responses repeated them), so take the
  values from the last line of the group.
- Other lines (tool-result `user` lines, `attachment`, `system`) can fall between two lines of one `message.id`
  when tool calls run in parallel, so group by `message.id`, not by adjacency.
- Groups seen: `thinking` first (usually), then `text` and/or one or more `tool_use`. A final answer has
  `stop_reason: "end_turn"`; a turn that calls tools has `"tool_use"`.
- Block shapes: `thinking` `{type, thinking, signature}`; `text` `{type, text}`; `tool_use`
  `{type, id, name, input, caller: {type: "direct"}}`. `input` is an object whose keys depend on the tool
  (Bash `command`, `description`; Read `file_path`, `offset`, `limit`; Write `file_path`, `content`; Edit
  `file_path`, `old_string`, `new_string`, `replace_all`; Skill `skill`; Agent `description`, `prompt`,
  `subagent_type`, `model`; ToolSearch `query`, `max_results`; MCP tools: the tool's own parameters). `name` is
  the tool name; MCP tools are `mcp__<server>__<tool>`.
- Extra assistant-line fields: `requestedModel`, `advisorModel`, `effort`, `perTurnEffort`, `thinkingDisplay`,
  `thinkingDurationMs`, `attributionSkill`/`attributionPlugin`/`attributionMcpServer`/`attributionMcpTool`,
  `wireToolInputs` and `wireIngestContext` (objects keyed by `toolu_...` ids, holding copies of tool inputs or the
  `cwd`).
- `usage` fields: `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`
  (integers), `cache_creation {ephemeral_1h_input_tokens, ephemeral_5m_input_tokens}`, `service_tier`,
  `inference_geo`, `speed`, `iterations[]`, `output_tokens_details {thinking_tokens}`, `server_tool_use
  {web_search_requests, web_fetch_requests}`.

## Compaction

A compaction writes two lines in a row, appended at the end of the file (earlier lines are not removed):

1. `system/compact_boundary`: `isMeta: false`, `level: "info"`, `content` string, **`parentUuid: null`**,
   `logicalParentUuid` (string, the `uuid` of an earlier `system` line just before the boundary), and
   `compactMetadata {trigger: "auto" (the only value seen), preTokens, postTokens, durationMs, cumulativeDroppedTokens,
   preCompactDiscoveredTools[], preservedSegment {headUuid, tailUuid, anchorUuid}, preservedMessages {uuids[],
   allUuids[], anchorUuid}}`. The preserved uuids point at lines that stay in the model's context; they
   all appear earlier in the file.
2. a `user` line with **`isCompactSummary: true`**, `isVisibleInTranscriptOnly: true`, `message.content` a string
   (the summary text), `parentUuid` equal to the boundary's `uuid`, and a `turnPosition`.

Lines after the summary chain from the summary (the first one has `parentUuid` equal to the summary's `uuid`).
The summary is model-written recap, not something a human typed, so a capture parser should not store it as a
prompt; it is also not a tool result. Both lines have a `slug` field.

## `attachment` lines

`attachment` is an object whose string `type` selects its shape. Seen: `hook_success`
(`hookEvent`, `hookName`, `command`, `exitCode`, `durationMs`, `stdout`, `stderr`, `content`, `toolUseID`),
`hook_additional_context`, `hook_system_message`, `environment` (`snapshot`, `changes`), `model`, `date`,
`file`, `edited_text_file`, `nested_memory`, `instructions`, `invoked_skills`, `skill_listing`,
`deferred_tools_delta`, `deferred_tools_record`, `agent_listing_delta`, `mcp_instructions_delta`,
`command_permissions`, `auto_mode`, `credential_org`, `remote_session_change`, `session_context`,
`prompt_snapshot`, `queued_command`, `total_tokens_reminder`, `silent_turn_reminder`, `compact_file_reference`,
`thinking_drop`. `hookEvent` values seen: `SessionStart`, `PreToolUse`, `PostToolUse`, `Stop`. Optional top-level
extras: `rendered` (array), `renderedRole`, `renderedInHumanTurn`, `renderedBesideToolResult`.
Attachments carry no conversation text a capture needs; skip them.

## Other line types

- `system/turn_duration`: `durationMs`, `messageCount`, `pendingBackgroundAgentCount?`, `isMeta` (false in all seen).
- `system/stop_hook_summary`: `hookCount`, `hookInfos[{command, durationMs}]`, `hookErrors[]`,
  `hookAdditionalContext[]`, `hasOutput`, `preventedContinuation`, `stopReason`, `toolUseID`, `level`.
- `system/informational`: `content` string, `level`, `isMeta` (false in all seen).
- `queue-operation`: `operation` (`enqueue`, `dequeue`, `remove`), `content?`, `commandUuid?`, `deliveryId?`,
  `reason?` (`absorbed_mid_turn`), `sessionId`, `timestamp`.
- `file-history-snapshot`: `messageId`, `isSnapshotUpdate`, `snapshot {messageId, timestamp, trackedFileBackups}`;
  `trackedFileBackups` is an object keyed by file path whose values are `{backupFileName, version, backupTime,
  realParentDir}`.
- `file-history-delta`: `messageId`, `snapshotMessageId`, `trackingPath`, `backup {backupFileName (string or null),
  backupTime, realParentDir, version}`, `timestamp`.
- `ai-title`: `aiTitle` string. `last-prompt`: `lastPrompt` string, `leafUuid`. `mode`: `mode` string.
  `permission-mode`: `permissionMode` string. `atis-latch`: `atis` string.
- `api-request-shape`, `api-request`, `api-request-blob`: raw request logging (system prompt, tool schemas,
  request params, content-addressed message blobs keyed by `hash`). Large and not conversation.

## Not in the fixtures

The fixture windows hold no `isSidechain: true` line in a main file (it is `true` only inside subagent files),
no `is_error: true` with an array `content`, and no `file-history-delta` with a non-null `backupFileName`.
`isMeta`, `isCompactSummary` and `compact_boundary` are present in both main fixtures; `isSidechain: true` is in
`sample-1-agent-1.jsonl`; an image block is in `sample-2.jsonl`.

## Windows (what the fixtures keep of the source sessions)

Each fixture is the concatenation of windows of 1-based source line numbers; the first column is the fixture line
range. Everything between two windows was dropped.

| fixture | fixture lines | source lines |
|---|---|---|
| `sample-1.jsonl` | 1-100 | 1-100 |
| | 101-111 | 185-195 |
| | 112-120 | 1190-1198 |
| | 121-189 | 1985-2053 (holds the compaction, fixture lines 168-169) |
| `sample-1-agent-1.jsonl` | 1-60 | 1-60 (whole subagent file starts here) |
| `sample-2.jsonl` | 1-100 | 1-100 |
| | 101-116 | 410-425 (holds an image result) |
| | 117-157 | 1320-1360 (holds the compaction, fixture lines 142-143) |

These effects are **artifacts of the windows, not features of the format**:

- At or just after the start of a window (windows start at `sample-1.jsonl` lines 101, 112 and 121, and at
  `sample-2.jsonl` lines 101 and 117) a line can have a `parentUuid` that points at a line that is not in the
  fixture (unresolved links are at `sample-1.jsonl` lines 101, 113, 122 and `sample-2.jsonl` lines 104, 117). In the
  whole session checked, every `parentUuid` resolved (the only `null`s were the first line and the compaction
  boundary).
- A `tool_use` at the end of one window can have its `tool_result` missing, and a `tool_result` at the start of a
  window can have no `tool_use` in the fixture.
- `compactMetadata.preservedSegment` and `preservedMessages` name uuids of earlier lines that were kept in the
  model context; not all of those lines are inside the fixture windows.
- `message.id` groups, `turnPosition` indexes and `apiBlockIndex` runs can be cut mid-group at a window edge.
- Repeated bookkeeping lines (`last-prompt`, `mode`, ...) appear fewer times than in a whole session.
- Strings are capped at 300 characters, arrays at 12 items and objects at 40 keys, which also shortens things like
  `preservedMessages.allUuids`, `addedNames` and `trackedFileBackups`.
- Timestamps are shifted into the past by one fixed offset (same offset for every fixture, so gaps are real)
  and are all before 2026-10-16.

## Parser checklist

1. Read line by line; skip a line that is not valid JSON; dispatch on `type` first and ignore unknown `type`
   (never on `message.role` alone). Tell an unknown `type` with none of the conversation keys above at any
   depth (bookkeeping) from one with such a key anywhere (a possible format change).
2. Conversation lines: `user`, `assistant`. Drop `isMeta: true`, `isCompactSummary: true`, `isSidechain: true` (main
   files should have none) and `tool_result` lines when you want typed prompts only.
3. Group `assistant` lines by `message.id`; take usage once.
4. Pair `tool_result.tool_use_id` with `tool_use.id`; do not rely on adjacency.
5. After a `system/compact_boundary`, the next `user` line with `isCompactSummary` is the recap.
6. Handle both `content` shapes (string or array of blocks) for `user` and for `tool_result`.
