# Example Plugins

The repository includes focused example plugins you can copy into your local
plugin directory.

## Included Examples

- `hello_plugin`
- `timer_plugin`
- `clock_usage_plugin`
- `api_tour_plugin`
- `event_callback_demo_plugin`
- `loader_progress_demo_plugin`
- `stateful_counter_plugin`
- `channel_toolbox_plugin`
- `ui_window_showcase_plugin`
- `ui_settings_demo_plugin`
- `ui_host_panels_demo_plugin`

## What They Show

- `hello_plugin`: the smallest possible command registration and completion example
- `timer_plugin`: delayed local work with `c2.later`
- `clock_usage_plugin`: a retained clock window with local time, session time, lifetime Crust time, and focused per-channel tracking
- `api_tour_plugin`: broad coverage of the Lua API surface, including account lookups, channel lookups, reply-capable chat actions, reward-redemption updates, fetch helpers, and event callbacks
- `event_callback_demo_plugin`: a focused callback example for profile, preview, and stream-status events
- `loader_progress_demo_plugin`: low-level loader notifications, including `ImagePrefetchQueued`, `EmoteImageReady`, and `EmoteCatalogUpdated`
- `stateful_counter_plugin`: persistent plugin state and completion hooks
- `channel_toolbox_plugin`: channel lookup, sending messages, local notes, and clearing message buffers
- `ui_window_showcase_plugin`: floating plugin window, host-form fields, submit events, retained updates, and a `link_button` that still emits callbacks; see [UI](./API_UI)
- `ui_settings_demo_plugin`: plugin-owned settings page in the shared Plugins area of the Settings window with host-form submit handling; see [UI](./API_UI)
- `ui_host_panels_demo_plugin`: plugin-owned host panels rendered in `settings.appearance`, `sidebar.top`, and `channel_header`; see [UI](./API_UI)

## Suggested Reading Order

1. `hello_plugin`
2. `timer_plugin`
3. `channel_toolbox_plugin`
4. `event_callback_demo_plugin`
5. `loader_progress_demo_plugin`
6. `ui_window_showcase_plugin`
7. `ui_settings_demo_plugin`
8. `ui_host_panels_demo_plugin`
9. `api_tour_plugin`

## Install

See [Installation](./Installation).

## Related Docs

1. [Plugin Installation](./Installation)
2. [Plugin Lifecycle](./Lifecycle)
3. [API Reference](./API)
