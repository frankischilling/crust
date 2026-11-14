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
- Functions usually return `nil` unless the page says otherwise.
- Read-only snapshots are plain Lua tables.
- Async work is callback-driven: call a `c2.fetch_*` helper or trigger an app
  action, then listen for the matching `c2.EventType.*` payload.
- Every callback payload table includes a `type` field.

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
