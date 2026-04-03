local function join_path(base, name)
  if base:sub(-1) == "/" then
    return base .. name
  end
  return base .. "/" .. name
end

local function fmt_duration(seconds)
  seconds = math.max(0, math.floor(seconds or 0))
  local days = math.floor(seconds / 86400)
  seconds = seconds % 86400
  local hours = math.floor(seconds / 3600)
  seconds = seconds % 3600
  local minutes = math.floor(seconds / 60)
  seconds = seconds % 60

  local parts = {}
  if days > 0 then table.insert(parts, days .. "d") end
  if hours > 0 or days > 0 then table.insert(parts, hours .. "h") end
  if minutes > 0 or hours > 0 or days > 0 then table.insert(parts, minutes .. "m") end
  table.insert(parts, seconds .. "s")
  return table.concat(parts, " ")
end

local function fmt_time(dt, use_24h)
  if use_24h then
    return string.format("%02d:%02d:%02d", dt.hour, dt.min, dt.sec)
  end

  local hour = dt.hour % 12
  if hour == 0 then
    hour = 12
  end
  local suffix = dt.hour >= 12 and "PM" or "AM"
  return string.format("%02d:%02d:%02d %s", hour, dt.min, dt.sec, suffix)
end

local function current_session_seconds()
  return math.max(0, math.floor((os.time() * 1000 - c2.session_started_ms()) / 1000))
end

local function get_clock_mode()
  if c2.use_24h_timestamps then
    return c2.use_24h_timestamps()
  end
  return true
end

local function describe_account()
  local account = c2.current_account()
  return string.format(
    "logged_in=%s username=%s display_name=%s user_id=%s",
    tostring(account.logged_in),
    tostring(account.username or ""),
    tostring(account.display_name or ""),
    tostring(account.user_id or "")
  )
end

local function describe_channel(channel)
  return string.format(
    "%s | platform=%s | joined=%s | mod=%s | vip=%s | broadcaster=%s",
    tostring(channel.display_name or channel.name or channel.id or ""),
    tostring(channel.platform or ""),
    tostring(channel.is_joined),
    tostring(channel.is_mod),
    tostring(channel.is_vip),
    tostring(channel.is_broadcaster)
  )
end

local function completion_demo(ev)
  if not ev.is_first_word then
    return { hide_others = false, values = {} }
  end

  local query = string.lower(ev.query or "")
  if query == "a" or query == "ap" or query == "api" then
    return {
      hide_others = true,
      values = {
        "apitour",
        "apitour info",
        "apitour account",
        "apitour channel",
        "apitour send",
        "apitour later",
        "apitour clear",
        "apitour open",
      },
    }
  end

  return { hide_others = false, values = {} }
end

local function api_tour(ctx)
  local mode = "info"
  if ctx.words and #ctx.words > 1 then
    mode = string.lower(ctx.words[2])
  end

  local plugin_dir = c2.plugin_dir() or "."
  local data_dir = c2.plugin_data_dir() or plugin_dir
  local now_local = os.date("*t")
  local use_24h = get_clock_mode()
  local session_seconds = current_session_seconds()

  if mode == "account" then
    c2.add_system_message(ctx.channel_name, "Account: " .. describe_account())
    return
  end

  if mode == "channel" then
    local target = ctx.words[3] or ctx.channel_name
    local channel = c2.channel_by_name(target)
    c2.add_system_message(ctx.channel_name, "Channel: " .. describe_channel(channel))
    return
  end

  if mode == "send" then
    local text = table.concat(ctx.words, " ", 3)
    if text == "" then
      text = "Hello from api_tour_plugin"
    end
    c2.send_message(ctx.channel, text)
    c2.add_system_message(ctx.channel_name, "Sent a chat message with c2.send_message")
    return
  end

  if mode == "clear" then
    c2.clear_messages(ctx.channel)
    c2.add_system_message(ctx.channel_name, "Cleared messages with c2.clear_messages")
    return
  end

  if mode == "open" then
    c2.open_url("https://github.com/frankischilling/crust")
    c2.add_system_message(ctx.channel_name, "Opened the Crust repository in your browser")
    return
  end

  if mode == "later" then
    c2.later(function()
      c2.add_system_message(
        ctx.channel_name,
        "Delayed callback fired after 2 seconds from api_tour_plugin"
      )
    end, 2000)
    c2.add_system_message(ctx.channel_name, "Queued a delayed callback with c2.later")
    return
  end

  c2.add_system_message(
    ctx.channel_name,
    "API tour | Time: " .. fmt_time(now_local, use_24h) ..
      " | Session: " .. fmt_duration(session_seconds) ..
      " | Local timezone: " .. tostring(os.date("%Z")) ..
      " | Plugin dir: " .. plugin_dir ..
      " | Data dir: " .. data_dir ..
      " | Account: " .. describe_account()
  )
end

c2.log(c2.LogLevel.Info, "API Tour Plugin loaded from " .. tostring(c2.plugin_dir()))

c2.register_callback(c2.EventType.CompletionRequested, completion_demo)

c2.register_command("apitour", api_tour, {
  usage = "/apitour [info|account|channel|send|clear|open|later]",
  summary = "Tour the Crust Lua API",
  aliases = { "api", "tour" },
})
