# Crust Plugin Examples

These example plugins are meant to be copied into your local Crust plugin
directory.

Crust does **not** load plugins from this repository path directly. It loads
plugins from your app data directory:

```text
~/.local/share/crust/plugins
```

On Windows, the default location is:

```text
%APPDATA%\crust\crust\data\plugins
```

## Examples

- [`hello_plugin`](./hello_plugin) - command registration and completions
- [`timer_plugin`](./timer_plugin) - delayed local messages with `c2.later`
- [`clock_usage_plugin`](./clock_usage_plugin) - retained clock window with session time, total Crust time, and focused per-channel tracking
- [`api_tour_plugin`](./api_tour_plugin) - a guided tour of the Lua API surface
- [`event_callback_demo_plugin`](./event_callback_demo_plugin) - focused callback and fetch-result examples
- [`loader_progress_demo_plugin`](./loader_progress_demo_plugin) - image/emote loader notifications and progress tracking
- [`stateful_counter_plugin`](./stateful_counter_plugin) - persistence with `plugin_data_dir()`
- [`channel_toolbox_plugin`](./channel_toolbox_plugin) - channel lookup and message helpers
- [`ui_window_showcase_plugin`](./ui_window_showcase_plugin) - floating retained plugin UI window with host-form fields, submit handling, and a `link_button`
- [`ui_settings_demo_plugin`](./ui_settings_demo_plugin) - retained plugin settings page with host-form submit handling
- [`ui_host_panels_demo_plugin`](./ui_host_panels_demo_plugin) - retained host panels rendered into `settings.appearance`, `sidebar.top`, and `channel_header`
- [`c9_api_expansion_demo`](./c9_api_expansion_demo) - exercises the filter engine, sound/hotkey snapshots, and upload events exposed by the C9 plugin API expansion

## Install

```text
mkdir -p ~/.local/share/crust/plugins
cp -r plugins/hello_plugin ~/.local/share/crust/plugins/
cp -r plugins/timer_plugin ~/.local/share/crust/plugins/
cp -r plugins/clock_usage_plugin ~/.local/share/crust/plugins/
cp -r plugins/api_tour_plugin ~/.local/share/crust/plugins/
cp -r plugins/event_callback_demo_plugin ~/.local/share/crust/plugins/
cp -r plugins/loader_progress_demo_plugin ~/.local/share/crust/plugins/
cp -r plugins/stateful_counter_plugin ~/.local/share/crust/plugins/
cp -r plugins/channel_toolbox_plugin ~/.local/share/crust/plugins/
cp -r plugins/ui_window_showcase_plugin ~/.local/share/crust/plugins/
cp -r plugins/ui_settings_demo_plugin ~/.local/share/crust/plugins/
cp -r plugins/ui_host_panels_demo_plugin ~/.local/share/crust/plugins/
cp -r plugins/c9_api_expansion_demo ~/.local/share/crust/plugins/
```

Then restart Crust or run `/reloadplugins`.

If you are writing your own plugin, start with the wiki at
[`docs/HOME.md`](../docs/HOME.md) and the API reference at
[`docs/API.md`](../docs/API.md).
