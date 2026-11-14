# Crust v0.3.0 release notes

Date: 2026-04-04

v0.3.0 adds Lua plugins for commands, callbacks, and custom UI. It follows
v0.2.0, which added whisper support.

## Highlights

- Lua plugin runtime support
- Plugin discovery and lifecycle management
- Slash-command registration and completion from plugins
- Host callback integration for events and timers
- Plugin-owned retained UI surfaces (floating windows and settings pages)

## Using plugins

- Write local plugins in Lua and load them from your plugin directory
- Register plugin commands that behave like native commands
- React to host events with callbacks for chat and UI interactions
- Build lightweight plugin UI without writing Rust code
- Persist plugin-owned state and data across sessions

## Getting started

1. Read [Plugin Installation](../Installation.md)
2. Review [Plugin Lifecycle](../Lifecycle.md)
3. Explore [Plugin API Reference](../API.md)
4. Try [Plugin Examples](../Examples.md)

## Notes

- Crust is still an early-stage project and APIs may evolve.
- Example plugins in this repository are the recommended starting point.
