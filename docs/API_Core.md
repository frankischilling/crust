# Core and callbacks

These helpers register commands and callbacks, read app state, and control
filters, uploads, sounds, and shortcuts.

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

Callback payloads are documented in [API_Events](./API_Events.md).

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

Reload all plugins from disk and rebuild the plugin registry. This calls the
host's reload command and does not return a payload table.

## Read-only helpers

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

## Filter expressions

Plugins can parse and evaluate the same typed expressions used by chat filters.
The [identifier definitions](../crates/core/src/filters/context.rs) list the
available names and types; the [parser](../crates/core/src/filters/parser.rs)
defines the grammar.

### `c2.filters_parse(expression)`

Parse and type-check a filter expression against the chat-message typing
context. Returns a table:

- on success: `{ ok = true, type = "Bool" }` (the `type` field is the
  label of the result type, usually `Bool`)
- on failure: `{ ok = false, error = "<message>" }`

### `c2.filters_evaluate(expression, context)`

Parse, type-check, and evaluate `expression` against a Lua table of
identifier -> value bindings.

- `expression`: filter expression string
- `context`: table keyed by identifier name (`author.login`,
  `message.content`, `channel.name`, `flags.reply`, ...). Values may be
  booleans, numbers, strings, or string-array tables.

Return values:

- on success: a boolean (the expression's truthiness)
- on parse / type error: `nil, error_message`

An identifier missing from the supplied `context` evaluates to `false`.
Supply the declared value type for each identifier used by the expression.

When Crust builds a message filter context, `flags.shared` and `flags.whisper`
reflect the message's Shared Chat and whisper metadata. A Lua caller passes
those values in its own `context` table.

## Uploader

### `c2.upload_image(channel, bytes_b64, format, source_path?)`

Queue an image upload through the configured uploader endpoint.

- `channel`: channel snapshot table (e.g. `ctx.channel`)
- `bytes_b64`: base64-encoded image bytes
- `format`: file extension without the dot (`"png"`, `"gif"`, `"jpeg"`)
- `source_path`: optional original on-disk path

The upload progresses asynchronously. Subscribe to
`c2.EventType.UploadStarted` and `c2.EventType.UploadFinished` to observe
progress; on success the resolved URL is also appended to the input
buffer of `channel` by the host.

## Sound events

### `c2.set_sound_settings(events)`

Replace the full per-event sound settings map. `events` is keyed by
event name (`mention`, `whisper`, `subscribe`, `raid`,
`custom_highlight`); each value is a table of `{ enabled, path, volume }`.
Missing keys fall back to the host default.

Subscribe to `c2.EventType.SoundSettingsUpdated` to observe the current
snapshot (emitted on startup and after every successful change).

### `c2.get_sound_settings()`

Return the current sound settings map in the same shape as the `events`
argument to `set_sound_settings`. Use this when a plugin loads after the startup
`SoundSettingsUpdated` event. If no snapshot has been cached, it returns host
defaults.

## Hotkeys

### `c2.set_hotkey_bindings(bindings)`

Replace the full hotkey binding map. `bindings` is keyed by action key
(`zoom_in`, `open_quick_switcher`, `next_tab`, ...); each value is a table
of `{ ctrl, shift, alt, command, key }`. Missing keys keep their prior
(or default) binding.

Subscribe to `c2.EventType.HotkeyBindingsUpdated` to observe the current
snapshot (emitted on startup and after every successful change).

### `c2.get_hotkey_bindings()`

Return the current shortcut map in the same shape as the `bindings` argument
to `set_hotkey_bindings`. Use this when a plugin loads after the startup
`HotkeyBindingsUpdated` event. If no snapshot has been cached, it returns the
built-in defaults.

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
