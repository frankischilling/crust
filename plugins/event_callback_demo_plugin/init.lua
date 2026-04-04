local stats = {
  auth = 0,
  profiles = 0,
  previews = 0,
  stream_updates = 0,
}

local function log_summary(prefix)
  c2.log(
    c2.LogLevel.Info,
    prefix ..
      string.format(
        " auth=%d profiles=%d previews=%d streams=%d",
        stats.auth,
        stats.profiles,
        stats.previews,
        stats.stream_updates
      )
  )
end

local function on_authenticated(ev)
  stats.auth = stats.auth + 1
  c2.log(c2.LogLevel.Info, "eventdemo authenticated as " .. tostring(ev.username or ""))
end

local function on_profile_loaded(ev)
  stats.profiles = stats.profiles + 1
  local profile = ev.profile or {}
  c2.log(
    c2.LogLevel.Info,
    "eventdemo profile " .. tostring(profile.display_name or profile.login or profile.id or "")
  )
end

local function on_link_preview_ready(ev)
  stats.previews = stats.previews + 1
  c2.log(
    c2.LogLevel.Info,
    "eventdemo preview " .. tostring(ev.site_name or "") .. " | " .. tostring(ev.title or ev.url or "")
  )
end

local function on_stream_status_updated(ev)
  stats.stream_updates = stats.stream_updates + 1
  c2.log(
    c2.LogLevel.Info,
    "eventdemo stream " .. tostring(ev.login or "") .. " live=" .. tostring(ev.is_live)
  )
end

local function eventdemo(ctx)
  local mode = "status"
  if ctx.words and #ctx.words > 1 then
    mode = string.lower(ctx.words[2])
  end

  if mode == "profile" then
    local login = ctx.words[3] or ctx.channel_name
    c2.fetch_user_profile(login)
    c2.add_system_message(ctx.channel_name, "Requested profile for " .. tostring(login))
    return
  end

  if mode == "preview" then
    local url = table.concat(ctx.words, " ", 3)
    if url == "" then
      url = "https://github.com/frankischilling/crust"
    end
    c2.fetch_link_preview(url)
    c2.add_system_message(ctx.channel_name, "Requested preview for " .. url)
    return
  end

  if mode == "stream" then
    local login = ctx.words[3] or ctx.channel_name
    c2.fetch_stream_status(login)
    c2.add_system_message(ctx.channel_name, "Requested stream status for " .. tostring(login))
    return
  end

  c2.add_system_message(
    ctx.channel_name,
    string.format(
      "Callbacks seen | auth=%d profile=%d preview=%d stream=%d",
      stats.auth,
      stats.profiles,
      stats.previews,
      stats.stream_updates
    )
  )
end

c2.log(c2.LogLevel.Info, "Event Callback Demo Plugin loaded")

c2.register_callback(c2.EventType.Authenticated, on_authenticated)
c2.register_callback(c2.EventType.UserProfileLoaded, on_profile_loaded)
c2.register_callback(c2.EventType.LinkPreviewReady, on_link_preview_ready)
c2.register_callback(c2.EventType.StreamStatusUpdated, on_stream_status_updated)

c2.register_command("eventdemo", eventdemo, {
  usage = "/eventdemo [status|profile <login>|preview <url>|stream <login>]",
  summary = "Show callback-driven fetch patterns",
})

log_summary("eventdemo ready |")
