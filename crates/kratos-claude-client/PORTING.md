# Native Claude client port

This crate replaces the SDK's subprocess transport for the first R4 slice. The engine's thread actor, permission policy, tool server implementation, usage scheduler, and API wiring remain later slices.

## API mapping

| TypeScript or SDK behavior | Rust owner |
| --- | --- |
| `query` options and process creation | `Launch`, `PermissionMode`, `SettingSource`, and `connect` |
| `initializationResult` | `Initialize` and `Initialized` |
| `inputFeed.push` and `ClaudeSession.submit` | `Claude::send` with the caller's UUID and JSON content |
| `sessionEvent` init and accepted events | `StreamMessage::Init` and `Replay` |
| `resultEvent` | `TurnResult::parse` and `TurnOutcome` |
| `Query.interrupt` | `Claude::interrupt`, with a control deadline |
| `Query.close` | `Claude::close` and last-handle cleanup through `OwnedChild` |
| `CanUseTool`, SDK MCP, hooks, and elicitation | `IncomingControl` and `ControlRequest` |
| `getSessionInfo` | `SessionStart::for_session` and `transcript_path` |
| `getSessionMessages` delivery UUIDs | `delivered_message_ids`, including native queued-command attachments |

`connect` takes a synchronous startup-control callback because the recorded CLI requests SDK MCP initialization before returning the main initialization response. The callback answers the owned control or hands it to an application task. It must return promptly. After initialization, the receiver owns subsequent controls. A transport does not invent application MCP responses.

`transcript_path` takes the operating-system user home, then appends `.claude/projects`. Its path encoding replaces each non-ASCII-alphanumeric character with `-`, as pinned by the Windows recording layout. `SessionId` validates UUIDs before they become filenames. An unreadable transcript or malformed recognized delivery marker is an error rather than evidence of non-delivery.

## Test mapping

Every test has a `Catches` comment naming the regression it detects. The test names provide the detailed mapping within these groups.

| Source behavior | Rust test file and scenarios |
| --- | --- |
| TS `sessionEvent`, replay acceptance, and `resultEvent` | `client.rs` and `recorded_protocol.rs`, s1, s2 sequential and fold, s4, and s4c |
| SDK initialization account shapes and subscription login | `recorded_protocol.rs`, s0 and s1 |
| Native session identity across process restart | `recorded_protocol.rs`, s5 launch flags and init frames |
| Native permission suggestions and post-tool hooks | `client.rs`, s3 |
| SDK MCP initialize, notifications, tools/list, tools/call, and permission source | `client.rs`, s6 |
| Echoed host control responses | `client.rs`, s11 with replay enabled |
| Structured report output | `recorded_protocol.rs`, s7 initialize-only |
| Plan-file write without callback, other-file Write callback, and bypass AskUserQuestion | `recorded_protocol.rs`, s9 |
| Cancelled asynchronous message lifecycle | `recorded_protocol.rs`, s2 cancel |
| Folded delivery markers after restart | `recorded_protocol.rs`, scrubbed native s2 transcript sample |
| Rate limits including five-hour and seven-day windows | `recorded_protocol.rs`, real s1 event |
| Drop answers, unsupported controls, withdrawal, elicitation, deadlines, process failure, bounded line recovery, cleanup, argv, and environment | `client.rs`, explicitly synthetic fake controls and helper subprocess |
| Failed-success text, missing error text, plural-empty precedence, empty singular UUID, empty model, malformed delivery markers, and unknown message kinds | `recorded_protocol.rs`, explicitly constructed boundary cases |
| No account identity, user path, or non-synthetic session ID in committed data | `transcripts_are_scrubbed.rs`, all recordings and planted negative fixtures |

## Deliberate differences

- There is no Agent SDK dependency or engine actor in this slice. It launches the native CLI directly with the measured stream-json flags.
- The public connection API adds a startup-control callback to accommodate the recorded bootstrap order. `Initialized` comes from the control response, while `SessionInit` comes from later streamed init frames.
- Typed account data excludes identity. The retained raw initialization response also removes account email and organization identifiers. Raw result data remains available for later usage consumers.
- A timeout reports an unknown remote outcome and never retries. The SDK's interrupt had no built-in timeout.
- Incoming controls answer an error on unanswered drop, and withdrawn controls can no longer answer. Unsupported control subtypes receive an error automatically.
- Unknown message kinds are retained. Malformed known frames produce `Incoming::Unreadable` and parsing continues. Oversized lines produce `Incoming::Oversized` with the complete byte count, then parsing continues at the next newline.
- The stdout line budget is 131,072 bytes. The measured maximum was 50,630 bytes in 1,089 messages across 34 spike recordings, excluding newline. The source is s8 settings isolation, project-and-local run, wire line 6.
- Incoming events, outgoing writes, and pending outgoing controls each have a capacity of 256. Published reverse controls reserve a write slot for their eventual answer. Exhausting reverse-response capacity ends the connection rather than allocating an unbounded queue.
- Consumers drain incoming events concurrently with live control requests. A full incoming queue pauses stdout and control replies while deadlines continue. Close and process-exit settlement remain independent of event consumption.
- `send` means admission to the writer, and only Replay means acceptance. Close drops queued writes when it closes stdin. It does not promise to flush or durably deliver them.
- Process exit settles pending requests independently of consumer backpressure. Exit includes up to the final 20 stderr lines within a 16 KiB byte tail. Close waits for process-tree completion rather than event consumption.
- Inherited `CLAUDE*`, `ANTHROPIC*`, and `KRATOS*` environment variables are removed case-insensitively before intentional `env_set` values are applied. Environment tests inject only synthetic values into a helper process, never the test runner.
- Executable discovery supports the native installation. A Windows npm or script launcher is reported with its path and an actionable native-installation diagnostic. It is not run through a shell. An explicit override is never silently replaced by another PATH entry.

## Preserved quirks

Abort reasons take precedence over Claude's error flag. An explicitly empty plural UUID list takes precedence over the singular field. A missing or empty singular field produces no message ID. Assistant correlation can disappear after a fold. Ordinary user tool-result frames do not acknowledge submission. The replay flag can echo the driver's own control responses, which are ignored when no host request is pending. A folded message is saved as `attachment.queued_command.source_uuid`, rather than another prompt row. Session flags depend on the transcript's existence because the CLI refuses each flag in the other's case.

## Verification

The implementation worker ran the focused crate and fake-agent suites, targeted Clippy with warnings denied, formatting, complexity at a maximum of 11, and library rustdoc. The final report records exact commands, counts, and independent root verification. No real Claude process, provider model call, credential operation, engine integration, or live R4 lane ran in this slice.
