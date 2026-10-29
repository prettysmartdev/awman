# Agent output fixtures

These sanitized, representative protocol transcripts were authored for WI 0137;
they are not claimed to be captures from an authenticated agent run. Claude is
not installed in the implementation environment. Codex's installed `exec --help`
confirms `--json` prints JSONL; no model/network call was made for fixtures.

- `claude_stream_json.ndjson`: init, assistant text/thinking/tool use, user tool
  results (string and content blocks), final assistant text and success result.
- `codex_json.ndjson`: thread/turn lifecycle, reasoning and interim message,
  command start/completion, file change, MCP call, web search and final message.

Both include the literal malformed line starting `malformed:` in the middle;
that line must survive unchanged as stdout. The final message in both is:

    Implemented structured output.
    Validation: cargo check passed.

The final message is emitted once (two stdout lines then FinalMessage). Claude
has 7 activities: thinking, message, tool_use, tool_result, tool_use, tool_result,
message. Codex has 9 activities: thinking, message, tool_use, tool_result,
tool_result, tool_use, tool_result, tool_result, message.

Later tests should also cover unknown JSON shapes, malformed known shapes,
stderr passthrough, duplicate terminal frames, EOF before terminal frame, empty
streams, Unicode summaries and tool payloads longer than 200 characters.
