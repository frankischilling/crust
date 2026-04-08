# Crust v0.3.0 Release Notes

Date: 2026-04-04

v0.3.0 introduces Lua support and plugin support for Crust.

The previous v0.2.0 release focused on whisper support. This release expands
Crust from chat-only workflows into plugin-driven customization and automation.

## Highlights

- Lua plugin runtime support
- Plugin discovery and lifecycle management
- Slash-command registration and completion from plugins
- Host callback integration for events and timers
- Plugin-owned retained UI surfaces (floating windows and settings pages)

## What This Enables

- Write local plugins in Lua and load them from your plugin directory
- Register plugin commands that behave like native commands
- React to host events with callbacks for chat and UI interactions
- Build lightweight plugin UI without writing Rust code
- Persist plugin-owned state and data across sessions

## Compared With v0.2.0

- v0.2.0: whisper support
- v0.3.0: Lua and plugin platform support

## Getting Started

1. Read [Plugin Installation](./Installation)
2. Review [Plugin Lifecycle](./Lifecycle)
3. Explore [Plugin API Reference](./API)
4. Try [Plugin Examples](./Examples)

## Notes

- Crust is still an early-stage project and APIs may evolve.
- Example plugins in this repository are the recommended starting point.