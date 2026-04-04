local function channel_label(channel)
  if type(channel) == "string" then
    return channel
  end
  if not channel then
    return "(unknown channel)"
  end
  return tostring(channel.display_name or channel.name or channel.id or "")
end

local function describe_channel(channel)
  if not channel then
    return "(channel lookup failed)"
  end
  return string.format(
    "%s | platform=%s | joined=%s | mod=%s | vip=%s | broadcaster=%s",
    channel_label(channel),
    tostring(channel.platform or ""),
    tostring(channel.is_joined),
    tostring(channel.is_mod),
    tostring(channel.is_vip),
    tostring(channel.is_broadcaster)
  )
end

local function complete_tools(ev)
  if not ev.is_first_word then
    return { hide_others = false, values = {} }
  end

  local query = string.lower(ev.query or "")
  if query == "c" or query == "ch" or query == "cha" then
    return {
      hide_others = true,
      values = {
        "chatbox",
        "chatbox info",
        "chatbox note",
        "chatbox say",
        "chatbox clear",
      },
    }
  end

  return { hide_others = false, values = {} }
end

local function chatbox_cmd(ctx)
  local mode = "info"
  if ctx.words and #ctx.words > 1 then
    mode = string.lower(ctx.words[2])
  end

  local target_name = ctx.words[3] or ctx.channel_name
  local target_channel = c2.channel_by_name(target_name) or target_name

  if mode == "info" then
    c2.add_system_message(ctx.channel_name, "Channel: " .. describe_channel(c2.channel_by_name(target_name)))
    return
  end

  if mode == "note" then
    local text = table.concat(ctx.words, " ", 4)
    if text == "" then
      text = "Local note from channel_toolbox_plugin"
    end
    c2.add_system_message(target_channel, text)
    c2.add_system_message(ctx.channel_name, "Added a local system message to " .. channel_label(target_channel))
    return
  end

  if mode == "say" then
    local text = table.concat(ctx.words, " ", 4)
    if text == "" then
      text = "Hello from channel_toolbox_plugin"
    end
    c2.send_message(target_channel, text)
    c2.add_system_message(ctx.channel_name, "Sent chat to " .. channel_label(target_channel))
    return
  end

  if mode == "clear" then
    c2.clear_messages(target_channel)
    c2.add_system_message(ctx.channel_name, "Cleared messages for " .. channel_label(target_channel))
    return
  end

  c2.add_system_message(
    ctx.channel_name,
    "Modes: info, note, say, clear | Target: " .. channel_label(target_channel)
  )
end

c2.log(c2.LogLevel.Info, "Channel Toolbox Plugin loaded")

c2.register_callback(c2.EventType.CompletionRequested, complete_tools)

c2.register_command("chatbox", chatbox_cmd, {
  usage = "/chatbox [info|note|say|clear] [channel] [text...]",
  summary = "Demonstrate channel lookup and message helpers",
  aliases = { "chanbox", "box" },
})
