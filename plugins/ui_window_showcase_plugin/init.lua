local state = {
  clicks = 0,
  last_name = "",
  theme = "ocean",
}

local function render_window(open)
  c2.ui.register_window({
    id = "showcase",
    title = "Plugin UI Showcase",
    open = open == true,
    scroll = true,
    default_width = 520,
    default_height = 460,
    children = {
      { type = "heading", text = "Declarative Plugin Window" },
      {
        type = "text",
        text = "This window is fully described in Lua and rendered by Rust/egui.",
        style = { text_role = "muted" }
      },
      {
        type = "group",
        title = "Controls",
        children = {
          {
            type = "text_input",
            id = "name",
            form_key = "name",
            host_form = true,
            placeholder = "Type a display name"
          },
          {
            type = "select",
            id = "theme",
            form_key = "theme",
            host_form = true,
            value = state.theme,
            options = {
              { label = "Ocean", value = "ocean" },
              { label = "Sunrise", value = "sunrise" },
              { label = "Forest", value = "forest" }
            }
          },
          {
            type = "checkbox",
            id = "pin",
            form_key = "pin",
            host_form = true,
            text = "Pretend to pin this window"
          },
          {
            type = "slider",
            id = "intensity",
            form_key = "intensity",
            host_form = true,
            min = 0,
            max = 100,
            value = 35,
            text = "Intensity"
          },
          {
            type = "row",
            children = {
              { type = "button", id = "increment", text = "Add Click", action = "increment" },
              { type = "button", id = "save", text = "Save Form", action = "save", submit = true },
              { type = "link_button", id = "docs", text = "Open API Docs", action = "docs", url = "https://github.com" }
            }
          }
        }
      },
      {
        type = "progress",
        text = "Click progress",
        progress = math.min(1, state.clicks / 10)
      },
      {
        type = "list",
        items = {
          { label = "Clicks", value = tostring(state.clicks) },
          { label = "Last saved name", value = state.last_name ~= "" and state.last_name or "(none)" },
          { label = "Theme", value = state.theme }
        }
      },
      {
        type = "table",
        columns = {
          { id = "field", title = "Field" },
          { id = "value", title = "Value" }
        },
        rows = {
          { "clicks", tostring(state.clicks) },
          { "theme", state.theme },
          { "saved_name", state.last_name ~= "" and state.last_name or "(none)" }
        }
      }
    }
  })
end

render_window(false)

c2.register_callback(c2.EventType.PluginUiAction, function(ev)
  if ev.surface_id ~= "showcase" then
    return
  end

  if ev.action == "increment" then
    state.clicks = state.clicks + 1
    render_window(true)
    return
  end

  if ev.action == "docs" then
    c2.add_system_message("system", "Docs link clicked. The browser navigation comes from the link button itself.")
    return
  end
end)

c2.register_callback(c2.EventType.PluginUiSubmit, function(ev)
  if ev.surface_id ~= "showcase" then
    return
  end

  if ev.form_values then
    state.last_name = tostring(ev.form_values.name or "")
    state.theme = tostring(ev.form_values.theme or state.theme)
  end
  state.clicks = state.clicks + 1
  c2.add_system_message("system", "Saved showcase form for " .. (state.last_name ~= "" and state.last_name or "anonymous"))
  render_window(true)
end)

c2.register_command("uishowcase", function(ctx)
  render_window(true)
  c2.ui.open_window("showcase")
  return "Opened the plugin UI showcase window."
end)
