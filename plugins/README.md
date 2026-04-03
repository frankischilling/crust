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
%APPDATA%\crust\plugins
```

## Examples

- [`hello_plugin`](./hello_plugin) - command registration and completions
- [`timer_plugin`](./timer_plugin) - delayed local messages with `c2.later`
- [`clock_usage_plugin`](./clock_usage_plugin) - date, time, timezone, session uptime, and overall runtime
- [`api_tour_plugin`](./api_tour_plugin) - a guided tour of the Lua API surface
- [`stateful_counter_plugin`](./stateful_counter_plugin) - persistence with `plugin_data_dir()`
- [`channel_toolbox_plugin`](./channel_toolbox_plugin) - channel lookup and message helpers

## Install

```text
mkdir -p ~/.local/share/crust/plugins
cp -r plugins/hello_plugin ~/.local/share/crust/plugins/
cp -r plugins/timer_plugin ~/.local/share/crust/plugins/
cp -r plugins/clock_usage_plugin ~/.local/share/crust/plugins/
cp -r plugins/api_tour_plugin ~/.local/share/crust/plugins/
cp -r plugins/stateful_counter_plugin ~/.local/share/crust/plugins/
cp -r plugins/channel_toolbox_plugin ~/.local/share/crust/plugins/
```

Then restart Crust or run `/reloadplugins`.

If you are writing your own plugin, start with the wiki at
[`docs/HOME.md`](../docs/HOME.md) and the API reference at
[`docs/API.md`](../docs/API.md).
