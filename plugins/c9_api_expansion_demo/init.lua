-- Demonstrates C9 plugin API expansion: filter engine, sound settings,
-- hotkey bindings, and uploader events.

local latest_sounds = nil
local latest_hotkeys = nil
local uploads_in_flight = 0

c2.register_callback(c2.EventType.SoundSettingsUpdated, function(ev)
  latest_sounds = ev.events
  c2.log(c2.LogLevel.Info, "c9demo: sound settings snapshot received")
end)

c2.register_callback(c2.EventType.HotkeyBindingsUpdated, function(ev)
  latest_hotkeys = ev.bindings
  c2.log(c2.LogLevel.Info, "c9demo: hotkey bindings snapshot received")
end)

c2.register_callback(c2.EventType.UploadStarted, function(ev)
  uploads_in_flight = uploads_in_flight + 1
  c2.log(c2.LogLevel.Info, "c9demo: upload started in " .. tostring(ev.channel and ev.channel.display_name or "?"))
end)

c2.register_callback(c2.EventType.UploadFinished, function(ev)
  uploads_in_flight = math.max(0, uploads_in_flight - 1)
  if ev.ok then
    c2.log(c2.LogLevel.Info, "c9demo: upload ok -> " .. tostring(ev.url))
  else
    c2.log(c2.LogLevel.Warning, "c9demo: upload failed: " .. tostring(ev.error))
  end
end)

local function cmd_filter(ctx)
  local expr = table.concat(ctx.words, " ", 2)
  if expr == "" then
    c2.add_system_message(ctx.channel, "usage: /c9filter <expression>")
    return
  end
  local parsed = c2.filters_parse(expr)
  if not parsed.ok then
    c2.add_system_message(ctx.channel, "parse error: " .. tostring(parsed.error))
    return
  end
  local sample = {
    ["author.login"] = ctx.account and ctx.account.username or "anonymous",
    ["author.subbed"] = false,
    ["message.content"] = ctx.raw_text or "",
    ["message.length"] = string.len(ctx.raw_text or ""),
    ["channel.name"] = ctx.channel_name,
    ["channel.live"] = false,
    ["has.link"] = false,
    ["has.mention"] = false,
  }
  local result, err = c2.filters_evaluate(expr, sample)
  if result == nil then
    c2.add_system_message(ctx.channel, "eval error: " .. tostring(err))
  else
    c2.add_system_message(
      ctx.channel,
      string.format("filter type=%s result=%s", parsed.type or "?", tostring(result))
    )
  end
end

local function cmd_sounds(ctx)
  local snapshot = latest_sounds or c2.get_sound_settings()
  if snapshot == nil then
    c2.add_system_message(ctx.channel, "c9demo: no sound snapshot yet")
    return
  end
  local parts = {}
  for key, setting in pairs(snapshot) do
    table.insert(
      parts,
      string.format(
        "%s(enabled=%s,vol=%.2f)",
        key,
        tostring(setting.enabled),
        setting.volume or 0
      )
    )
  end
  table.sort(parts)
  c2.add_system_message(ctx.channel, "sounds: " .. table.concat(parts, ", "))
end

local function cmd_hotkeys(ctx)
  local snapshot = latest_hotkeys or c2.get_hotkey_bindings()
  if snapshot == nil then
    c2.add_system_message(ctx.channel, "c9demo: no hotkey snapshot yet")
    return
  end
  local shown = 0
  local parts = {}
  for action, binding in pairs(snapshot) do
    shown = shown + 1
    if shown <= 4 then
      local mods = {}
      if binding.ctrl then table.insert(mods, "Ctrl") end
      if binding.shift then table.insert(mods, "Shift") end
      if binding.alt then table.insert(mods, "Alt") end
      table.insert(mods, binding.key or "")
      table.insert(parts, action .. "=" .. table.concat(mods, "+"))
    end
  end
  c2.add_system_message(
    ctx.channel,
    string.format("hotkeys (%d total): %s", shown, table.concat(parts, ", "))
  )
end

local function cmd_upload_status(ctx)
  c2.add_system_message(
    ctx.channel,
    "uploads in flight: " .. tostring(uploads_in_flight)
  )
end

c2.register_command("c9filter", cmd_filter, {
  usage = "/c9filter <expression>",
  summary = "Parse + evaluate a Chatterino filter expression",
})

c2.register_command("c9sounds", cmd_sounds, {
  usage = "/c9sounds",
  summary = "Dump latest sound settings snapshot",
})

c2.register_command("c9hotkeys", cmd_hotkeys, {
  usage = "/c9hotkeys",
  summary = "Dump first few hotkey bindings",
})

c2.register_command("c9uploads", cmd_upload_status, {
  usage = "/c9uploads",
  summary = "Show current upload count",
})

c2.log(c2.LogLevel.Info, "C9 API Expansion Demo loaded")
