# UI API

`c2.ui` exposes retained declarative plugin UI.

Crust renders the UI in Rust/egui. Lua only describes surfaces and reacts to
events.

## Namespace

```lua
c2.ui.register_window(spec)
c2.ui.update_window(id, spec)
c2.ui.open_window(id)
c2.ui.close_window(id)
c2.ui.unregister_window(id)

c2.ui.register_settings_page(spec)
c2.ui.update_settings_page(id, spec)
c2.ui.unregister_settings_page(id)

c2.ui.register_host_panel(spec)
c2.ui.update_host_panel(id, spec)
c2.ui.unregister_host_panel(id)
```

## Surfaces

### Window spec

```lua
{
  id = "showcase",
  title = "Plugin Window",
  open = true,
  resizable = true,
  scroll = false,
  default_width = 520,
  default_height = 420,
  min_width = 320,
  min_height = 220,
  max_width = 900,
  max_height = 700,
  style = { ... },
  children = { ... }
}
```

Rules:

- Only `id` is required. All other window-spec fields are optional.
- `title` defaults to `id`.
- `open` defaults to `true`.
- `resizable` defaults to `true`.
- `scroll` defaults to `false`.
- `default_width`, `default_height`, `min_width`, `min_height`, `max_width`, and
  `max_height` are optional numeric window bounds and size hints.
- `style` is optional.
- `max_width` and `max_height` are window bounds, not style-table hints.
- `children` is the retained widget tree and defaults to an empty list when omitted.
- Each plugin only mutates its own windows.

### Settings-page spec

```lua
{
  id = "demo_settings",
  title = "Settings Demo",
  summary = "Optional short summary shown in Settings",
  style = { ... },
  children = { ... }
}
```

Rules:

- Only `id` is required. All other settings-page fields are optional.
- `title` defaults to `id`.
- `summary` is optional.
- `style` is optional.
- `children` defaults to an empty list when omitted.
- Settings pages appear inside the shared Plugins area of the Settings window.

### Host-panel spec

```lua
{
  id = "appearance_tools",
  slot = "settings.appearance",
  title = "Plugin Controls",
  summary = "Optional short description for the plugin-owned block",
  order = 100,
  style = { ... },
  children = { ... }
}
```

Rules:

- `id` and `slot` are required. All other host-panel fields are optional.
- `slot` must be one of the documented host extension points.
- `title` is optional; when present, Crust renders it as the block heading.
- `summary` is optional helper text under the heading.
- `order` defaults to `0`; lower numbers render earlier within the same slot.
- `style` is optional.
- `children` defaults to an empty list when omitted.
- Host panels render as separate plugin-owned blocks inside stable Crust UI slots.

Supported `slot` values:

- `settings.integrations`
- `settings.appearance`
- `settings.chat`
- `sidebar.top`
- `channel_header`

## Functions

### `c2.ui.register_window(spec)`

Registers or replaces one floating plugin-owned window.

### `c2.ui.update_window(id, spec)`

Replaces the retained window with id `id`. The first argument is the source of
truth for the window id.

### `c2.ui.open_window(id)`

Marks a registered window as open. Unknown window ids are ignored.

### `c2.ui.close_window(id)`

Marks a registered window as closed. Unknown window ids are ignored.

### `c2.ui.unregister_window(id)`

Removes a retained window.

### `c2.ui.register_settings_page(spec)`

Registers or replaces one plugin-owned settings page.

### `c2.ui.update_settings_page(id, spec)`

Replaces the retained settings page with id `id`. The first argument is the
source of truth for the settings-page id.

### `c2.ui.unregister_settings_page(id)`

Removes a retained settings page.

### `c2.ui.register_host_panel(spec)`

Registers or replaces one plugin-owned host panel in a named Crust UI slot.

### `c2.ui.update_host_panel(id, spec)`

Replaces the retained host panel with id `id`. The first argument is the source
of truth for the host-panel id.

### `c2.ui.unregister_host_panel(id)`

Removes a retained host panel.

## Widget Schema

Every widget is a plain Lua table. `type` is required; the rest of the common
fields are optional unless a widget-specific note says otherwise.

```lua
{
  type = "button",
  id = "save",
  title = "Optional heading/title text",
  text = "Optional display text",
  action = "optional_action_name",
  url = "https://example.invalid",
  placeholder = "placeholder text",
  value = "string|boolean|number|array-of-strings",
  progress = 0.5,
  min = 0,
  max = 100,
  step = 1,
  rows = { ... },
  children = { ... },
  options = { ... },
  items = { ... },
  columns = { ... },
  form_key = "optional_form_field_name",
  host_form = true,
  submit = false,
  open = true,
  style = { ... }
}
```

Common widget notes:

- Unknown widget types are ignored.
- `id` is optional globally, but strongly recommended for action routing and
  callback handling.
- `children`, `rows`, `options`, `items`, and `columns` are only used by the
  widget types that declare them.
- `open` is only used by widgets such as `collapsible`.

### Layout widgets

- `column`: vertical stack of `children`
- `row`: wrapped horizontal layout of `children`
- `group`: framed container for `children`
- `card`: same retained schema as `group`, styled as a framed card
- `grid`: grid layout for `children`
- `scroll`: vertical scroll area around `children`
- `separator`: horizontal separator line
- `spacer`: vertical gap; uses `style.height` or `style.min_height`
- `collapsible`: collapsible section; `title` or `text` is the header

### Display widgets

- `text`: plain label text
- `heading`: larger section heading
- `label`: same display path as `text`
- `badge`: filled badge/pill label
- `image`: image URL from `url` or `style.image_url`
- `progress`: progress bar from `progress` or numeric `value`

### Action widgets

- `button`
- `icon_button`
- `link_button`

Action widget notes:

- `id` is strongly recommended.
- `action` is optional but useful for routing callback logic.
- `submit = true` emits `PluginUiSubmit` instead of `PluginUiAction`.
- `link_button` uses `url` for the destination and still emits a plugin UI event.
- `icon_button` currently uses the same renderer as `button`.
- `style.icon` is currently treated as fallback button text, not a separate icon slot.

### Input widgets

- `text_input`
- `text_area`
- `password_input`
- `checkbox`
- `toggle`
- `radio_group`
- `select`
- `slider`

Input widget notes:

- `radio_group` and `select` use `options`.
- `slider` uses numeric `min`, `max`, and optional positive `step`.
- `value` is the controlled value when `host_form` is false.
- `id` or `form_key` is required if you expect change or submit payloads to be useful.

### Structured display widgets

- `list`: uses `items`
- `table`: uses `columns` and `rows`

## Nested Value Tables

### `options`

```lua
{
  label = "Ocean",
  value = "ocean",
  description = "Optional help text"
}
```

### `items`

String shorthand is allowed:

```lua
items = { "alpha", "beta" }
```

Full form:

```lua
{
  label = "Clicks",
  value = "3",
  note = "Optional note"
}
```

### `columns`

```lua
{
  id = "field",
  title = "Field",
  align = "left"
}
```

### `rows`

`table.rows` is an array of arrays. Each cell may be a string, boolean,
number, or array of strings.

## Style Table

`style` is accepted by surfaces and widgets. All style fields are optional.

```lua
{
  visible = true,
  enabled = true,
  width = 240,
  height = 32,
  min_width = 120,
  min_height = 24,
  max_width = 640,
  max_height = 480,
  padding = 8,
  align = "left|center|right",
  text_role = "muted",
  emphasis = "strong|bold|small",
  border_color = "#RRGGBB",
  fill_color = "#RRGGBB" or "#RRGGBBAA",
  severity = "info|success|warning|danger|error",
  icon = "optional icon text",
  image_url = "https://example.invalid/image.png"
}
```

Current host behavior:

- `visible` hides windows, settings pages, and widgets when `false`
- `enabled` disables interactive widgets and can disable all children of a
  window or settings page
- `width` and `height` are currently used as rendering hints for text inputs,
  buttons, images, progress bars, and spacers
- `min_height` is used by `spacer`; `min_width` / `min_height` window bounds
  live on the window spec instead
- `max_width` and `max_height` inside `style` are currently accepted but do
  not usually affect widget layout
- `padding`, `align`, `border_color`, and table-column `align` are currently
  accepted but may not affect rendering
- `fill_color` / `severity` currently affect grouped containers,
  settings-page frames, badges, and styled text
- `text_role` / `emphasis` affect text rendering
- unsupported hints are ignored safely

## State Model

### Controlled mode

When `host_form` is omitted or `false`:

- Lua owns the widget value through `value`
- Crust emits `PluginUiChange`
- if the plugin does not re-register the updated retained value, the widget falls back to the old retained value on the next render
- the plugin should call `c2.ui.update_window(...)` or
  `c2.ui.update_settings_page(...)` with the new retained value

### Host-form mode

When `host_form = true`:

- Crust stores transient field values for that surface
- `id` or `form_key` identifies the form field
- change events still fire
- submit/action events include `form_values`
- only host-form fields are included in `form_values`
- transient host-form state is cleaned up when the surface disappears

## UI Events

See [API_Events.md](./API_Events) for the callback payload reference.  

Plugin UI callback payloads use `surface_kind`, `surface_id`, `widget_id`, and
`form_values` consistently across action, change, and submit events.

`surface_kind` is currently:

- `window`
- `settings_page`
- `host_panel`

The UI-related event kinds are:

- `c2.EventType.PluginUiAction`
- `c2.EventType.PluginUiChange`
- `c2.EventType.PluginUiSubmit`
- `c2.EventType.PluginUiWindowClosed`

## Example

```lua
c2.ui.register_window({
  id = "demo",
  title = "Demo Window",
  open = false,
  children = {
    { type = "heading", text = "Hello UI" },
    {
      type = "text_input",
      id = "name",
      form_key = "name",
      host_form = true,
      placeholder = "Name"
    },
    {
      type = "button",
      id = "save",
      text = "Save",
      action = "save",
      submit = true
    }
  }
})

c2.register_command("uidemo", function(ctx)
  c2.ui.open_window("demo")
end)

c2.register_callback(c2.EventType.PluginUiSubmit, function(ev)
  if ev.surface_id ~= "demo" then
    return
  end
  c2.add_system_message("system", "Saved name: " .. tostring(ev.form_values.name or ""))
end)
```

For fuller examples, see
[`plugins/ui_window_showcase_plugin/init.lua`](../plugins/ui_window_showcase_plugin/init.lua)
and
[`plugins/ui_settings_demo_plugin/init.lua`](../plugins/ui_settings_demo_plugin/init.lua),
plus
[`plugins/ui_host_panels_demo_plugin/init.lua`](../plugins/ui_host_panels_demo_plugin/init.lua).

## Related Docs

1. [Events](./API_Events)
2. [Examples](./Examples)
