# Lua API Reference

This page documents the Crust-specific Lua host API.

## Global Tables

### `c2.LogLevel`

```lua
c2.LogLevel.Debug
c2.LogLevel.Info
c2.LogLevel.Warning
c2.LogLevel.Critical
```

### `c2.EventType`

```lua
c2.EventType.CompletionRequested
```

## Logging And Commands

### `c2.log(level, ...parts)`

Write a message to the Crust log.

### `c2.register_command(name, handler, meta?)`

Register a slash command.

Arguments:

- `name`: command name, with or without the leading `/`
- `handler`: Lua function called for the command
- `meta`: optional table with:
  - `usage`
  - `summary`
  - `aliases`

Command context fields:

- `command`
- `raw_text`
- `channel`
- `channel_name`
- `account`
- `words`
- `reply_to_msg_id`
- `reply`

Return values:

- `nil` to do nothing
- a string to post a local system message

### `c2.register_callback(event_type, handler)`

Register a callback for host-driven events.

Currently supported event:

- `c2.EventType.CompletionRequested`

Completion event fields:

- `query`
- `full_text_content`
- `cursor_position`
- `is_first_word`
- `channel`

Completion result fields:

- `values`
- `hide_others`

### `c2.later(callback, msec)`

Run a callback later on the plugin VM.

## Account And Channel

### `c2.current_account()`

Return the current Twitch account snapshot.

### `c2.channel_by_name(name)`

Return a channel info table for a channel name.

### `c2.send_message(channel, text)`

Send a chat message.

`channel` may be:

- a channel name string
- a channel table from `c2.channel_by_name`

### `c2.add_system_message(channel, text)`

Inject a local system message into the visible channel feed.

### `c2.clear_messages(channel)`

Clear the visible message buffer for a channel.

### `c2.open_url(url)`

Open a URL in the system browser.

## Paths And Time

### `c2.plugin_dir()`

Return the plugin installation directory.

### `c2.plugin_data_dir()`

Return the writable per-plugin data directory.

Use this for persistent state, caches, and timestamps.

### `c2.use_24h_timestamps()`

Return `true` when Crust is configured to render timestamps in 24-hour time.

### `c2.session_started_ms()`

Return the current Crust session start time in Unix milliseconds.

Use this to compute session uptime without relying on wall-clock persistence.

## Example

```lua
c2.log(c2.LogLevel.Info, "plugin loaded")

c2.register_command("hello", function(ctx)
  c2.add_system_message(ctx.channel_name, "Hello from Lua")
end, {
  usage = "/hello",
  summary = "Print a local message",
})
```
