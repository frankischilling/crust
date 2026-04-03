# Installation

Crust loads plugins from the app data `plugins` directory.

Typical locations:

- Linux: `~/.local/share/crust/plugins`
- Windows: `%APPDATA%\crust\plugins`
- macOS: `~/Library/Application Support/crust/plugins`

Each direct child directory is treated as a plugin candidate.

Example layout:

```text
plugins/
└── my_plugin/
    ├── info.json
    ├── init.lua
    └── data/
```

Install the bundled examples:

```text
mkdir -p ~/.local/share/crust/plugins
cp -r plugins/hello_plugin ~/.local/share/crust/plugins/
cp -r plugins/timer_plugin ~/.local/share/crust/plugins/
cp -r plugins/clock_usage_plugin ~/.local/share/crust/plugins/
cp -r plugins/api_tour_plugin ~/.local/share/crust/plugins/
cp -r plugins/stateful_counter_plugin ~/.local/share/crust/plugins/
cp -r plugins/channel_toolbox_plugin ~/.local/share/crust/plugins/
```

Windows PowerShell equivalent:

```powershell
New-Item -ItemType Directory -Force "$env:APPDATA\crust\plugins" | Out-Null
Copy-Item -Recurse plugins\hello_plugin "$env:APPDATA\crust\plugins\"
Copy-Item -Recurse plugins\timer_plugin "$env:APPDATA\crust\plugins\"
Copy-Item -Recurse plugins\clock_usage_plugin "$env:APPDATA\crust\plugins\"
Copy-Item -Recurse plugins\api_tour_plugin "$env:APPDATA\crust\plugins\"
Copy-Item -Recurse plugins\stateful_counter_plugin "$env:APPDATA\crust\plugins\"
Copy-Item -Recurse plugins\channel_toolbox_plugin "$env:APPDATA\crust\plugins\"
```

Reload plugins with `/reloadplugins` or use the Settings page.
