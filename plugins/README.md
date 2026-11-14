# Crust plugin examples

Copy the example folders you want to use into Crust's plugin directory.
The default Linux location is:

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
- [`clock_usage_plugin`](./clock_usage_plugin) - a clock window showing session time, total Crust time, and time spent focused on each channel
- [`api_tour_plugin`](./api_tour_plugin) - a guided tour of the Lua API surface
- [`event_callback_demo_plugin`](./event_callback_demo_plugin) - callbacks for profile, link-preview, and stream-status requests
- [`loader_progress_demo_plugin`](./loader_progress_demo_plugin) - image/emote loader notifications and progress tracking
- [`stateful_counter_plugin`](./stateful_counter_plugin) - persistence with `plugin_data_dir()`
- [`channel_toolbox_plugin`](./channel_toolbox_plugin) - channel lookup and message helpers
- [`ui_window_showcase_plugin`](./ui_window_showcase_plugin) - a floating window with form fields, submit handling, and a `link_button`
- [`ui_settings_demo_plugin`](./ui_settings_demo_plugin) - a settings page with form submission
- [`ui_host_panels_demo_plugin`](./ui_host_panels_demo_plugin) - panels in `settings.appearance`, `sidebar.top`, and `channel_header`
- [`c9_api_expansion_demo`](./c9_api_expansion_demo) - filters, sound and shortcut settings, and upload events

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

To write a plugin, start with the documentation index at
[`docs/HOME.md`](../docs/HOME.md) and the API reference at
[`docs/API.md`](../docs/API.md).
