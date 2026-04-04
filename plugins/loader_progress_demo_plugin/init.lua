local progress = {
  queued = 0,
  ready = 0,
  catalogs = 0,
}

local function remaining_images()
  return math.max(0, progress.queued - progress.ready)
end

local function on_image_prefetch_queued(ev)
  progress.queued = progress.queued + math.max(0, tonumber(ev.count) or 0)
  c2.log(
    c2.LogLevel.Info,
    "loadwatch queued total=" .. tostring(progress.queued) .. " remaining=" .. tostring(remaining_images())
  )
end

local function on_emote_image_ready(ev)
  progress.ready = progress.ready + 1
  c2.log(
    c2.LogLevel.Info,
    "loadwatch ready uri=" .. tostring(ev.uri or "") .. " remaining=" .. tostring(remaining_images())
  )
end

local function on_emote_catalog_updated(ev)
  progress.catalogs = progress.catalogs + 1
  local count = ev.emotes and #ev.emotes or 0
  c2.log(
    c2.LogLevel.Info,
    "loadwatch catalog update " .. tostring(progress.catalogs) .. " size=" .. tostring(count)
  )
end

local function loadwatch(ctx)
  local mode = "status"
  if ctx.words and #ctx.words > 1 then
    mode = string.lower(ctx.words[2])
  end

  if mode == "reset" then
    progress.queued = 0
    progress.ready = 0
    progress.catalogs = 0
    c2.add_system_message(ctx.channel_name, "Loader progress counters reset")
    return
  end

  if mode == "fetch" then
    local url = table.concat(ctx.words, " ", 3)
    if url == "" then
      url = "https://static-cdn.jtvnw.net/emoticons/v2/25/default/light/3.0"
    end
    c2.fetch_image(url)
    c2.add_system_message(ctx.channel_name, "Requested image fetch for " .. url)
    return
  end

  c2.add_system_message(
    ctx.channel_name,
    string.format(
      "Loader progress | queued=%d ready=%d remaining=%d catalogs=%d",
      progress.queued,
      progress.ready,
      remaining_images(),
      progress.catalogs
    )
  )
end

c2.log(c2.LogLevel.Info, "Loader Progress Demo Plugin loaded")

c2.register_callback(c2.EventType.ImagePrefetchQueued, on_image_prefetch_queued)
c2.register_callback(c2.EventType.EmoteImageReady, on_emote_image_ready)
c2.register_callback(c2.EventType.EmoteCatalogUpdated, on_emote_catalog_updated)

c2.register_command("loadwatch", loadwatch, {
  usage = "/loadwatch [status|reset|fetch <url>]",
  summary = "Track image-prefetch and emote-loader events",
})
