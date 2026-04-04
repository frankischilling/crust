local state = {
  note = "No settings saved yet.",
}

local function render_page()
  c2.ui.register_settings_page({
    id = "demo_settings",
    title = "Settings Demo",
    summary = "Example plugin-owned settings inside Crust Settings -> Integrations.",
    children = {
      { type = "heading", text = "Plugin Settings Page" },
      { type = "text", text = state.note, style = { text_role = "muted" } },
      {
        type = "group",
        title = "Preferences",
        children = {
          {
            type = "text_input",
            id = "nickname",
            form_key = "nickname",
            host_form = true,
            placeholder = "Nickname"
          },
          {
            type = "toggle",
            id = "alerts",
            form_key = "alerts",
            host_form = true,
            text = "Enable alerts",
            value = true
          },
          {
            type = "radio_group",
            id = "layout",
            form_key = "layout",
            host_form = true,
            value = "compact",
            options = {
              { label = "Compact", value = "compact" },
              { label = "Expanded", value = "expanded" }
            }
          },
          {
            type = "button",
            id = "apply",
            text = "Apply Settings",
            action = "apply",
            submit = true
          }
        }
      }
    }
  })
end

render_page()

c2.register_callback(c2.EventType.PluginUiSubmit, function(ev)
  if ev.surface_id ~= "demo_settings" then
    return
  end

  local nickname = ev.form_values and tostring(ev.form_values.nickname or "") or ""
  local alerts = ev.form_values and tostring(ev.form_values.alerts or false) or "false"
  local layout = ev.form_values and tostring(ev.form_values.layout or "compact") or "compact"
  state.note = string.format(
    "Saved nickname=%s alerts=%s layout=%s",
    nickname ~= "" and nickname or "(none)",
    alerts,
    layout
  )
  c2.add_system_message("system", state.note)
  render_page()
end)
