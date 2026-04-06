# Lifecycle

Plugin load order:

1. Crust scans the plugin directory.
2. Each direct child directory is treated as a plugin candidate.
3. Crust reads `info.json`.
4. Crust creates a Lua VM for that plugin.
5. Crust registers the `c2.*` host API.
6. Crust runs the entry script, usually `init.lua`.
7. The plugin registers commands, callbacks, and delayed work during init.

Manifest fields:

- `name`
- `description`
- `authors`
- `homepage`
- `tags`
- `version`
- `license`
- `permissions`
- `entry`

Notes:

- `entry` defaults to `init.lua`.
- `permissions` are metadata only for now.
- Plugins reload as a whole directory, not file-by-file.
- Use `plugin_data_dir()` for writable state.

Directory layout:

```text
plugins/
└-- example_plugin/
    ├-- info.json
    ├-- init.lua
    ├-- helper.lua
    └-- data/
```

Lua module loading:

- `package.path` is extended so `require("helper")` can load `helper.lua`.
- `require("nested/module")` can load `nested/module.lua` or `nested/module/init.lua`.

Reloading:

- Use `/reloadplugins`.
- Crust also exposes a reload button in Settings.
