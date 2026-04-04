# Core And Callbacks

This page covers the shared helpers every plugin usually touches first.

## `c2.log(level, ...parts)`

Write a line to the Crust log.

- `level` is one of `c2.LogLevel.Debug`, `Info`, `Warning`, or `Critical`
- `parts` are converted to strings and joined with spaces

## `c2.register_command(name, handler, meta?)`

Register a slash command.

- `name`: command name with or without the leading `/`
- `handler`: function called as `handler(ctx)`
- `meta`: optional table with `usage`, `summary`, and `aliases`

The command handler context table includes:

- `command`: normalized command name
- `raw_text`: original command text
- `channel_name`: display name of the current channel
- `channel`: channel snapshot table
- `account`: current account snapshot table
- `words`: tokenized command words
- `reply_to_msg_id`: omitted when the command was not invoked as a reply
- `reply`: omitted when reply metadata is unavailable

The `reply` table includes:

- `parent_msg_id`
- `parent_user_login`
- `parent_display_name`
- `parent_msg_body`

Return behavior:

- return `nil` to do nothing
- return a string to inject a local system message into the current channel

## `c2.register_callback(event_type, handler)`

Register a Lua callback for host-driven events.

- `event_type`: a value from `c2.EventType` or the matching event-type string
- `handler`: function called as `handler(ev)`

Callback payloads are documented in [API_Events](./API_Events).

Completion callbacks are special:

- register for `c2.EventType.CompletionRequested`
- return `{ values = { ... }, hide_others = false }`

The completion payload includes:

- `query`
- `full_text_content`
- `cursor_position`
- `is_first_word`
- `channel`: omitted when completion is requested without channel context

## `c2.later(callback, msec)`

Schedule `callback()` to run later on the plugin VM.

- `callback`: zero-argument Lua function
- `msec`: delay in milliseconds

## `c2.reload_plugins()`

Reload all plugins from disk.

- this is an action helper, not a fetch helper
- it causes the host to rebuild the plugin registry from the current plugin
  directories
- it does not add a new Crust capability beyond the existing Rust reload
  command
- it does not return a payload table

## Read-Only Helpers

## `c2.plugin_dir()`

Return the installed plugin directory path as a string.

## `c2.plugin_data_dir()`

Return the writable per-plugin data directory path as a string.

## `c2.use_24h_timestamps()`

Return `true` when Crust is currently configured for 24-hour timestamps.

## `c2.session_started_ms()`

Return the current Crust session start time in Unix milliseconds.

## `c2.current_account()`

Return the current account snapshot table.

Fields:

- `logged_in`: always present
- `username`: omitted when no authenticated account is active
- `user_id`: omitted when no authenticated account is active
- `display_name`: omitted when no authenticated account is active

`username`, `user_id`, and `display_name` are omitted when no authenticated
account is active.

## `c2.current_channel()`

Return the currently active channel snapshot table, or `nil` when Crust does
not currently have an active channel.

In split view, this follows the focused pane. Otherwise it follows the active
channel tab.

## `c2.channel_by_name(name)`

Return a channel snapshot table for the provided channel name.

- `name` may be a Twitch login, Kick channel, or IRC name

Channel snapshot fields:

- `name`: channel display name as exposed by `ChannelId::display_name()`
- `display_name`: same value as `name`
- `platform`: one of the values listed below
- `id`: canonical channel identifier string
- `is_twitch`
- `is_irc`
- `is_kick`
- `is_joined`: present when the host has a snapshot for the channel
- `is_mod`: present when the host has a snapshot for the channel
- `is_vip`: present when the host has a snapshot for the channel
- `is_broadcaster`: present when the host has a snapshot for the channel

`platform` is one of:

- `twitch`
- `kick`
- `irc`

## Example

```lua
c2.log(c2.LogLevel.Info, "plugin loaded from", c2.plugin_dir())

c2.register_command("hello", function(ctx)
  local account = c2.current_account()
  local active_channel = c2.current_channel()
  c2.add_system_message(
    ctx.channel,
    "Hello from " .. tostring(account.display_name or account.username or "Lua") ..
      " in " .. tostring(active_channel and active_channel.display_name or "(no active channel)")
  )
end, {
  usage = "/hello",
  summary = "Print a local message",
  aliases = { "hi" },
})
```
