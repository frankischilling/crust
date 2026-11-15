# Lua API reference

This reference describes the Lua functions, UI definitions, and callback payloads
exposed by Crust's Rust host. The pages are grouped by task.

## Pages

- [Core and callbacks](./API_Core.md)
- [UI](./API_UI.md)
- [Chat and channels](./API_Chat.md)
- [Accounts and settings](./API_Accounts_Settings.md)
- [Moderation and creator tools](./API_Moderation.md)
- [Events](./API_Events.md)
- [Examples](./Examples.md)

## Global tables and namespaces

### `c2.LogLevel`

```lua
c2.LogLevel.Debug
c2.LogLevel.Info
c2.LogLevel.Warning
c2.LogLevel.Critical
```

### `c2.EventType`

`c2.EventType` contains every event kind accepted by
`c2.register_callback(event_type, handler)`.

The current Lua bridge includes:

- completion events
- account and connection events
- chat and channel events
- fetch-result events
- moderation and settings events
- loader and emote/image events, including `ImagePrefetchQueued`

Plugins receive the events exported by the Lua bridge. Some runtime events are
internal and are not forwarded to plugins.

The command reference covers actions exposed to plugins. `RunPluginCommand`
and `RunPluginCallback` are internal host commands.

The event reference lists shared payload tables, optional fields, and accepted
string values.

### `c2.ui`

`c2.ui` is the declarative retained UI namespace for plugin-owned floating
windows and plugin settings pages.

See [API_UI.md](./API_UI.md).

## API conventions

- All functions live under the global `c2` table.
- Declarative UI helpers live under `c2.ui`.
- Helpers that queue app actions return the command result described below.
  Other return values are documented on each helper's page.
- Read-only snapshots are plain Lua tables.
- Async work is callback-driven: call a `c2.fetch_*` helper or trigger an app
  action, then listen for the matching `c2.EventType.*` payload.
- Every callback payload table includes a `type` field.

## Command results and retries

Action helpers such as `c2.send_message`, `c2.ban_user`, `c2.join_channel`,
settings updates, and fetch requests try to queue their work immediately.
With valid arguments, they return:

| Result | Meaning |
| --- | --- |
| `true` | Crust accepted the action into its command queue. |
| `nil, reason` | The queue is full or the background service has stopped. The action was not queued. |

`reason` is a human-readable explanation that a plugin can display or log.
Calls that reject malformed arguments before queuing may return `nil` without
a reason; check the individual helper's argument requirements.

Acceptance only confirms that the action was queued. Network requests and
other work finish asynchronously; use the matching result events where
available. Crust does not retry or replay a rejected action. Keep its input
and let the user try again later, once Crust can accept work.

This example makes one attempt each time the user runs `/send_pending`.
If Crust is busy, the channel and message stay in `pending`; running the same
command later retries them. After acceptance, the command has nothing left
to send.

```lua
local pending = { channel = "some_channel", text = "Hello from a plugin" }

c2.register_command("send_pending", function()
  if not pending then return end

  local accepted, reason = c2.send_message(pending.channel, pending.text)
  if accepted then
    pending = nil
  else
    c2.log(c2.LogLevel.Warning, reason or "Check the channel and message arguments.")
  end
end)
```

[`c2.later`](./API_Core.md#c2latercallback-msec) schedules a callback and returns
`nil`. Action helpers called inside a delayed callback return the same command
results as calls made elsewhere.

## Common patterns

### Register a command

```lua
c2.register_command("hello", function(ctx)
  c2.add_system_message(ctx.channel, "Hello from Lua")
end)
```

### Register a callback

```lua
c2.register_callback(c2.EventType.Authenticated, function(ev)
  c2.log(c2.LogLevel.Info, "logged in as " .. tostring(ev.username or ""))
end)
```

### Fetch something and wait for the event

```lua
c2.register_callback(c2.EventType.UserProfileLoaded, function(ev)
  c2.log(c2.LogLevel.Info, "loaded profile for " .. tostring(ev.profile.login or ""))
end)

c2.fetch_user_profile("some_login")
```
