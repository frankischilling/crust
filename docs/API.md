# Lua API Reference

This is the exhaustive reference for the Lua API currently emitted and accepted
by the Rust host. The pages are split by category for navigation, but the goal
of this wiki is parity with the Lua surface that exists today.

## Pages

- [Core and callbacks](./API_Core)
- [UI](./API_UI)
- [Chat and channels](./API_Chat)
- [Accounts and settings](./API_Accounts_Settings)
- [Moderation and creator tools](./API_Moderation)
- [Events](./API_Events)
- [Examples](./Examples)

## Global Tables And Namespaces

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

The event surface now matches the user-facing `AppEvent` variants used by the
runtime.

The command surface also matches the non-internal `AppCommand` variants that
the runtime exposes to plugins. Internal host plumbing, such as
`RunPluginCommand` and `RunPluginCallback`, is intentionally not documented as
public Lua API.

The event reference also documents shared nested payload tables, optional-field
omission rules, and concrete string/value domains already enforced by the Rust
bridge.

### `c2.ui`

`c2.ui` is the declarative retained UI namespace for plugin-owned floating
windows and plugin settings pages.

See [API_UI.md](./API_UI).

## API Conventions

- All functions live under the global `c2` table.
- Declarative UI helpers live under `c2.ui`.
- Functions usually return `nil` unless the page says otherwise.
- Read-only snapshots are plain Lua tables.
- Async work is callback-driven: call a `c2.fetch_*` helper or trigger an app
  action, then listen for the matching `c2.EventType.*` payload.
- Every callback payload table includes a `type` field.

## Common Patterns

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
