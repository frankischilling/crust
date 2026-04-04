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

local function describe_profile(profile)
  return string.format(
    "%s | login=%s | live=%s | followers=%s",
    tostring(profile.display_name or profile.login or profile.id or ""),
    tostring(profile.login or ""),
    tostring(profile.is_live),
    tostring(profile.followers or 0)
  )
end

local function describe_stream(ev)
  return string.format(
    "%s | live=%s | title=%s | game=%s | viewers=%s",
    tostring(ev.login or ""),
    tostring(ev.is_live),
    tostring(ev.title or ""),
    tostring(ev.game or ""),
    tostring(ev.viewers or 0)
  )
end

local function describe_preview(ev)
  return string.format(
    "%s | title=%s | site=%s",
    tostring(ev.url or ""),
    tostring(ev.title or ""),
    tostring(ev.site_name or "")
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
        "apitour reply",
        "apitour ui",
        "apitour later",
        "apitour clear",
        "apitour open",
      },
    }
  end

  return { hide_others = false, values = {} }
end

local function log_event(prefix, details)
  c2.log(c2.LogLevel.Info, prefix .. " | " .. details)
end

local function on_authenticated(ev)
  log_event("event", "authenticated as " .. tostring(ev.username or ""))
end

local function on_profile_loaded(ev)
  log_event("event", "profile " .. describe_profile(ev.profile or {}))
end

local function on_stream_status(ev)
  log_event("event", "stream " .. describe_stream(ev))
end

local function on_link_preview_ready(ev)
  log_event("event", "link preview " .. describe_preview(ev))
end

local function on_emote_catalog_updated(ev)
  local count = ev.emotes and #ev.emotes or 0
  log_event("event", "emote catalog updated with " .. tostring(count) .. " entries")
end

local function on_emote_image_ready(ev)
  log_event(
    "event",
    string.format(
      "image %s | %sx%s | bytes(base64)=%s",
      tostring(ev.uri or ""),
      tostring(ev.width or 0),
      tostring(ev.height or 0),
      tostring(ev.raw_bytes_base64 and #ev.raw_bytes_base64 or 0)
    )
  )
end

local function on_image_prefetch_queued(ev)
  log_event("event", "image prefetch queued count=" .. tostring(ev.count or 0))
end

local function on_user_messages_cleared(ev)
  log_event(
    "event",
    "cleared visible messages for " .. tostring(ev.login or "") .. " in " .. tostring(ev.channel and ev.channel.name or "")
  )
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

  if mode == "ui" then
    c2.add_system_message(
      ctx.channel_name,
      "UI examples: ui_window_showcase_plugin (/uishowcase) and ui_settings_demo_plugin (Settings -> Integrations)"
    )
    return
  end

  if mode == "reply" then
    if not ctx.reply_to_msg_id or not ctx.reply then
      c2.add_system_message(
        ctx.channel_name,
        "Reply mode needs reply context. Use /apitour reply from a reply-enabled command context."
      )
      return
    end

    local text = table.concat(ctx.words, " ", 3)
    if text == "" then
      text = "Reply from api_tour_plugin"
    end

    c2.send_message(ctx.channel, text, {
      reply_to_msg_id = ctx.reply_to_msg_id,
      reply = ctx.reply,
    })
    c2.add_system_message(ctx.channel_name, "Sent a reply with c2.send_message(..., opts)")
    return
  end

  if mode == "clear" then
    c2.clear_messages(ctx.channel)
    c2.add_system_message(ctx.channel_name, "Cleared messages with c2.clear_messages")
    return
  end

  if mode == "join" then
    local target = ctx.words[3] or ctx.channel_name
    c2.join_channel(target)
    c2.add_system_message(ctx.channel_name, "Requested join for " .. tostring(target))
    return
  end

  if mode == "leave" then
    local target = ctx.words[3] or ctx.channel_name
    c2.leave_channel(target)
    c2.add_system_message(ctx.channel_name, "Requested leave for " .. tostring(target))
    return
  end

  if mode == "whisper" then
    local target = ctx.words[3]
    local text = table.concat(ctx.words, " ", 4)
    if target and text ~= "" then
      c2.send_whisper(target, text)
      c2.add_system_message(ctx.channel_name, "Queued a whisper to " .. target)
    else
      c2.add_system_message(ctx.channel_name, "Usage: /apitour whisper <login> <text>")
    end
    return
  end

  if mode == "card" then
    local target = ctx.words[3] or ctx.channel_name
    c2.show_user_card(target, ctx.channel)
    c2.add_system_message(ctx.channel_name, "Opened a user card for " .. tostring(target))
    return
  end

  if mode == "profile" then
    local target = ctx.words[3] or ctx.channel_name
    c2.fetch_user_profile(target)
    c2.add_system_message(ctx.channel_name, "Requested a user profile fetch for " .. tostring(target))
    return
  end

  if mode == "stream" then
    local target = ctx.words[3] or ctx.channel_name
    c2.fetch_stream_status(target)
    c2.add_system_message(ctx.channel_name, "Requested a stream status fetch for " .. tostring(target))
    return
  end

  if mode == "preview" then
    local url = table.concat(ctx.words, " ", 3)
    if url == "" then
      url = "https://github.com/frankischilling/crust"
    end
    c2.fetch_link_preview(url)
    c2.add_system_message(ctx.channel_name, "Requested a link preview for " .. url)
    return
  end

  if mode == "moderation" then
    c2.open_moderation_tools(ctx.channel)
    c2.add_system_message(ctx.channel_name, "Opened moderation tools for this channel")
    return
  end

  if mode == "reward" then
    local reward_id = ctx.words[3] or "reward-id"
    local redemption_id = ctx.words[4] or "redemption-id"
    local status = string.upper(ctx.words[5] or "FULFILLED")
    local user_login = ctx.words[6] or "some_user"
    local reward_title = table.concat(ctx.words, " ", 7)
    if reward_title == "" then
      reward_title = "Sample reward"
    end
    c2.update_reward_redemption_status(
      ctx.channel,
      reward_id,
      redemption_id,
      status,
      user_login,
      reward_title
    )
    c2.add_system_message(
      ctx.channel_name,
      "Requested reward redemption update for " .. reward_title
    )
    return
  end

  if mode == "settings" then
    c2.add_system_message(
      ctx.channel_name,
      string.format(
        "Settings | 24h=%s | session_started_ms=%s",
        tostring(c2.use_24h_timestamps()),
        tostring(c2.session_started_ms())
      )
    )
    return
  end

  if mode == "open" then
    c2.open_url("https://github.com/frankischilling/crust")
    c2.add_system_message(ctx.channel_name, "Opened the Crust repository in your browser")
    return
  end

  if mode == "reload" then
    c2.reload_plugins()
    c2.add_system_message(ctx.channel_name, "Requested plugin reload")
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

c2.register_callback(c2.EventType.Authenticated, on_authenticated)
c2.register_callback(c2.EventType.EmoteCatalogUpdated, on_emote_catalog_updated)
c2.register_callback(c2.EventType.EmoteImageReady, on_emote_image_ready)
c2.register_callback(c2.EventType.ImagePrefetchQueued, on_image_prefetch_queued)
c2.register_callback(c2.EventType.UserProfileLoaded, on_profile_loaded)
c2.register_callback(c2.EventType.StreamStatusUpdated, on_stream_status)
c2.register_callback(c2.EventType.LinkPreviewReady, on_link_preview_ready)
c2.register_callback(c2.EventType.UserMessagesCleared, on_user_messages_cleared)
c2.register_callback(c2.EventType.CompletionRequested, completion_demo)

c2.register_command("apitour", api_tour, {
  usage = "/apitour [info|account|channel|send|reply|clear|join|leave|whisper|card|profile|stream|preview|moderation|reward|settings|open|reload|later]",
  summary = "Tour the Crust Lua API",
  aliases = { "api", "tour" },
})
