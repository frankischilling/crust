local state = {
  note = "No host-panel actions yet.",
  accent = "Ocean",
}

local function render_panels()
  c2.ui.register_host_panel({
    id = "appearance_tools",
    slot = "settings.appearance",
    title = "Appearance Tools",
    summary = "Plugin-owned controls appended to the Appearance section.",
    order = 50,
    children = {
      { type = "text", text = state.note, style = { text_role = "muted" } },
      {
        type = "select",
        id = "accent_choice",
        form_key = "accent_choice",
        host_form = true,
        value = state.accent,
        options = {
          { label = "Ocean", value = "Ocean" },
          { label = "Sunset", value = "Sunset" },
          { label = "Forest", value = "Forest" }
        }
      },
      {
        type = "button",
        id = "apply_appearance",
        text = "Apply Accent",
        action = "apply_appearance",
        submit = true
      }
    }
  })

  c2.ui.register_host_panel({
    id = "sidebar_quick_actions",
    slot = "sidebar.top",
    title = "Quick Actions",
    summary = "Small plugin block above the channel list.",
    order = 25,
    children = {
      {
        type = "row",
        children = {
          { type = "badge", text = state.accent },
          {
            type = "button",
            id = "sidebar_ping",
            text = "Ping",
            action = "sidebar_ping"
          }
        }
      }
    }
  })

  c2.ui.register_host_panel({
    id = "active_channel_tools",
    slot = "channel_header",
    title = "Active Channel Tools",
    summary = "Plugin block above the active channel header bar.",
    order = 25,
    children = {
      {
        type = "row",
        children = {
          { type = "text", text = state.note, style = { text_role = "muted" } },
          {
            type = "button",
            id = "header_checkpoint",
            text = "Checkpoint",
            action = "header_checkpoint"
          }
        }
      }
    }
  })
end

render_panels()

c2.register_command("hostpanelsdemo", function(ctx)
  c2.add_system_message(
    "system",
    "UI Host Panels Demo is active in Settings -> Appearance, the sidebar, and the channel header."
  )
end)

c2.register_callback(c2.EventType.PluginUiAction, function(ev)
  if ev.surface_kind ~= "host_panel" then
    return
  end

  if ev.action == "sidebar_ping" then
    state.note = "Sidebar action clicked."
    c2.add_system_message("system", state.note)
    render_panels()
    return
  end

  if ev.action == "header_checkpoint" then
    local active = c2.current_channel()
    local label = active and active.login or "(no active channel)"
    state.note = "Checkpoint saved for " .. tostring(label)
    c2.add_system_message("system", state.note)
    render_panels()
  end
end)

c2.register_callback(c2.EventType.PluginUiSubmit, function(ev)
  if ev.surface_kind ~= "host_panel" or ev.surface_id ~= "appearance_tools" then
    return
  end

  local chosen = ev.form_values and tostring(ev.form_values.accent_choice or state.accent) or state.accent
  state.accent = chosen
  state.note = "Applied accent preset: " .. chosen
  c2.add_system_message("system", state.note)
  render_panels()
end)
