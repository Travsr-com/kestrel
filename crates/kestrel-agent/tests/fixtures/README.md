# Fixture streams

These streams are **doc-derived, not recorded.** Each line was written by hand from the documented (or
source-defined) output format. Neither agent was run against a real model to make them. Replace them with
recorded output once a run through the Kestrel gateway is possible.

One JSON object per line, as each runtime writes to stdout.

## Claude Code (`claude-code/`)

Launch: `claude --bare -p --output-format stream-json --verbose --model <model> --permission-mode bypassPermissions <input>`.

| File | What it covers |
| :--- | :--- |
| `success.jsonl` | `system/init`, assistant text and `tool_use`, user `tool_result` (array and string content, one `is_error: true`), a `system/api_retry`, a thinking block, final `result` with `is_error: false` |
| `error-api.jsonl` | Final `result` with `subtype: "success"` but `is_error: true` (API failure) |
| `error-max-turns.jsonl` | Final `result` with `subtype: "error_max_turns"` and an `errors` array, no `result` field |

Sources:

* Message types (`SDKSystemMessage`, `SDKAssistantMessage`, `SDKUserMessage`, `SDKResultMessage` and its
  error subtypes): <https://code.claude.com/docs/en/agent-sdk/typescript>
* `-p`, `--output-format stream-json`, `--verbose`, `--bare`, `system/api_retry`, last line is `result`:
  <https://code.claude.com/docs/en/headless> and <https://code.claude.com/docs/en/cli-reference>
* Content blocks inside `message` (`text`, `tool_use`, `tool_result`, `thinking`) follow the Messages API:
  <https://docs.claude.com/en/api/messages>

`error-api.jsonl` follows a shape checked locally with Claude Code 2.1.328 pointed at an unreachable base
URL (no request left the machine): the run failed, yet the result kept `subtype: "success"` and set
`is_error: true`. The real message text was reworded because it contains a dash this repo bans.

## OpenCode (`opencode/`)

Launch: `opencode run --standalone --format json --auto --model anthropic/<model> <input>`.

| File | What it covers |
| :--- | :--- |
| `success.jsonl` | `step_start`, `text`, `tool_use` (one `completed`, one `error`), `step_finish` with reason `tool-calls`, then a final step ending with reason `stop` |
| `error.jsonl` | An `error` event with `{ type, message }` |

OpenCode V2 docs do not describe the JSON events, so the shapes come from the emitter in the source:

* Events and fields: `renderer()` in
  <https://github.com/anomalyco/opencode/blob/dev/packages/cli/src/run/noninteractive.ts>. Each line is
  `{ type, timestamp, sessionID, ...data }`.
* Flags (`--format json`, `--model`, `--auto`, `--standalone`): <https://opencode.ai/docs/cli/commands/> and
  the V1 flag table at <https://opencode.ai/v1/docs/cli/>
* Gateway URL via `providers.anthropic.settings.baseURL`: <https://opencode.ai/docs/providers/>
* Inline config via `OPENCODE_CONFIG_CONTENT`: V1 env table at <https://opencode.ai/v1/docs/cli/>, still
  read in V2 by `packages/core/src/config.ts` in the same repo.

The `tokens` object inside `step_finish` is not typed in that file. Its shape here is a guess and no test
depends on it.

OpenCode has no final "result" line. A successful run ends with a `step_finish` whose reason is `stop`. A
failed run writes an `error` line and exits with code 1.
