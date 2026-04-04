local state = {
  compact_badge = false,
  accent = "ocean",
  sidebar_clicks = 0,
  note = "Host panels are rendered in stable Crust-owned slots.",
}

local function accent_label(value)
  if value == "ember" then
    return "Ember"
  end
  if value == "forest" then
    return "Forest"
  end
  return "Ocean"
end

local function render_panels()
  c2.ui.register_host_panel({
    id = "appearance_demo",
    slot = "settings.appearance",
    title = "Host Panel Demo",
    summary = "Plugin-owned controls rendered after native Appearance settings.",
    order = 50,
    children = {
      { type = "text", text = state.note, style = { text_role = "muted" } },
      {
        type = "select",
        id = "accent",
        text = "Accent Preset",
        value = state.accent,
        options = {
          { label = "Ocean", value = "ocean" },
          { label = "Forest", value = "forest" },
          { label = "Ember", value = "ember" }
        }
      },
      {
        type = "checkbox",
        id = "compact_badge",
        text = "Use compact channel-header badge",
        value = state.compact_badge
      },
      {
        type = "button",
        id = "reset_demo",
        text = "Reset Demo State",
        action = "reset_demo"
      }
    }
  })

  c2.ui.register_host_panel({
    id = "sidebar_demo",
    slot = "sidebar.top",
    title = "Quick Tools",
    summary = "Compact plugin block pinned above the channel list.",
    order = 10,
    children = {
      {
        type = "badge",
        text = "Clicks " .. tostring(state.sidebar_clicks),
        style = { severity = "info" }
      },
      {
        type = "button",
        id = "sidebar_ping",
        text = "Ping",
        action = "sidebar_ping"
      }
    }
  })

  c2.ui.register_host_panel({
    id = "channel_header_demo",
    slot = "channel_header",
    title = "Channel Tools",
    summary = "Plugin block rendered under the active channel info bar.",
    order = 10,
    children = {
      {
        type = "badge",
        text = state.compact_badge and "Compact" or "Expanded",
        style = { severity = "success" }
      },
      {
        type = "text",
        text = "Accent preset: " .. accent_label(state.accent),
        style = { text_role = "muted" }
      }
    }
  })
end

render_panels()

c2.register_callback(c2.EventType.PluginUiChange, function(ev)
  if ev.surface_kind ~= "host_panel" then
    return
  end

  if ev.surface_id == "appearance_demo" then
    if ev.widget_id == "accent" then
      state.accent = tostring(ev.value or "ocean")
      state.note = "Accent preset set to " .. accent_label(state.accent) .. "."
      render_panels()
      return
    end

    if ev.widget_id == "compact_badge" then
      state.compact_badge = ev.value == true
      state.note = state.compact_badge
        and "Compact header badge enabled."
        or "Compact header badge disabled."
      render_panels()
      return
    end
  end
end)

c2.register_callback(c2.EventType.PluginUiAction, function(ev)
  if ev.surface_kind ~= "host_panel" then
    return
  end

  if ev.action == "sidebar_ping" then
    state.sidebar_clicks = state.sidebar_clicks + 1
    c2.add_system_message("system", "Host panel ping " .. tostring(state.sidebar_clicks))
    render_panels()
    return
  end

  if ev.action == "reset_demo" then
    state.compact_badge = false
    state.accent = "ocean"
    state.sidebar_clicks = 0
    state.note = "Host panels are rendered in stable Crust-owned slots."
    render_panels()
  end
end)
