local function join_path(base, name)
  if base:sub(-1) == "/" then
    return base .. name
  end
  return base .. "/" .. name
end

local plugin_base_dir = c2.plugin_data_dir() or c2.plugin_dir() or "."
local state_file = join_path(plugin_base_dir, "clock_usage_state.txt")
local session_started_ms = c2.session_started_ms()
local session_started_unix = math.floor(session_started_ms / 1000)
local persist_interval_seconds = 5
local tick_interval_ms = 1000

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

local function fmt_duration(seconds)
  seconds = math.max(0, math.floor(seconds or 0))
  local days = math.floor(seconds / 86400)
  seconds = seconds % 86400
  local hours = math.floor(seconds / 3600)
  seconds = seconds % 3600
  local minutes = math.floor(seconds / 60)
  seconds = seconds % 60

  local parts = {}
  if days > 0 then
    table.insert(parts, days .. "d")
  end
  if hours > 0 or days > 0 then
    table.insert(parts, hours .. "h")
  end
  if minutes > 0 or hours > 0 or days > 0 then
    table.insert(parts, minutes .. "m")
  end
  table.insert(parts, seconds .. "s")
  return table.concat(parts, " ")
end

local function read_state(path)
  local f = io.open(path, "r")
  if not f then
    return {
      stored_session_started_ms = session_started_ms,
      overall_total_seconds = 0,
      channels = {},
    }
  end

  local out = {
    stored_session_started_ms = session_started_ms,
    overall_total_seconds = 0,
    channels = {},
  }

  local first_line = f:read("*l")
  local second_line = f:read("*l")
  out.stored_session_started_ms = tonumber(first_line) or session_started_ms
  out.overall_total_seconds = math.max(0, math.floor(tonumber(second_line) or 0))

  for line in f:lines() do
    local id, name, seconds = line:match("^([^\t]+)\t([^\t]*)\t(%d+)$")
    if id and seconds then
      out.channels[id] = {
        id = id,
        name = name ~= "" and name or id,
        seconds = math.max(0, math.floor(tonumber(seconds) or 0)),
      }
    end
  end

  f:close()
  return out
end

local function write_state(path, state)
  local f = io.open(path, "w")
  if not f then
    return false
  end

  f:write(tostring(session_started_ms))
  f:write("\n")
  f:write(tostring(math.max(0, math.floor(state.overall_total_seconds or 0))))
  f:write("\n")

  local ids = {}
  for id, _ in pairs(state.channels) do
    table.insert(ids, id)
  end
  table.sort(ids)

  for _, id in ipairs(ids) do
    local entry = state.channels[id]
    f:write(id)
    f:write("\t")
    f:write(tostring(entry.name or id))
    f:write("\t")
    f:write(tostring(math.max(0, math.floor(entry.seconds or 0))))
    f:write("\n")
  end

  f:close()
  return true
end

local state = {
  overall_total_seconds = 0,
  channels = {},
  last_sample_unix = nil,
  last_persist_unix = nil,
  show_all_channels = false,
  window_open = false,
  registered = false,
  initialized = false,
  ticking = false,
}

local function session_seconds_now()
  return math.max(0, os.time() - session_started_unix)
end

local function use_24h()
  if c2.use_24h_timestamps then
    return c2.use_24h_timestamps()
  end
  return true
end

local function current_channel_label()
  local channel = c2.current_channel and c2.current_channel() or nil
  if not channel then
    return "(none)", nil
  end

  local id = tostring(channel.id or channel.name or channel.display_name or "")
  local name = tostring(channel.display_name or channel.name or id)
  if id == "" then
    return "(none)", nil
  end
  return name, {
    id = id,
    name = name,
  }
end

local function ensure_channel_entry(channel_info)
  if not channel_info or not channel_info.id then
    return nil
  end

  local existing = state.channels[channel_info.id]
  if not existing then
    existing = {
      id = channel_info.id,
      name = channel_info.name or channel_info.id,
      seconds = 0,
    }
    state.channels[channel_info.id] = existing
  else
    existing.name = channel_info.name or existing.name or channel_info.id
  end
  return existing
end

local function ensure_initialized()
  if state.initialized then
    return
  end

  local persisted = read_state(state_file)
  state.overall_total_seconds = persisted.overall_total_seconds + session_seconds_now()
  state.channels = persisted.channels or {}
  state.last_sample_unix = os.time()
  state.last_persist_unix = persisted.stored_session_started_ms ~= session_started_ms and 0 or os.time()
  state.initialized = true
end

local function sample_usage()
  ensure_initialized()
  local now = os.time()
  local delta = math.max(0, now - (state.last_sample_unix or now))
  state.last_sample_unix = now

  if delta > 0 then
    state.overall_total_seconds = state.overall_total_seconds + delta

    local _, channel_info = current_channel_label()
    local channel_entry = ensure_channel_entry(channel_info)
    if channel_entry then
      channel_entry.seconds = channel_entry.seconds + delta
    end
  end

  if now - (state.last_persist_unix or 0) >= persist_interval_seconds then
    if write_state(state_file, state) then
      state.last_persist_unix = now
    end
  end
end

local function sorted_channel_entries()
  ensure_initialized()
  local rows = {}
  for _, entry in pairs(state.channels) do
    table.insert(rows, {
      id = entry.id,
      name = entry.name or entry.id,
      seconds = math.max(0, math.floor(entry.seconds or 0)),
    })
  end

  table.sort(rows, function(a, b)
    if a.seconds == b.seconds then
      return string.lower(a.name) < string.lower(b.name)
    end
    return a.seconds > b.seconds
  end)

  return rows
end

local function build_channel_rows(current_name)
  local entries = sorted_channel_entries()
  local rows = {}
  local limit = state.show_all_channels and #entries or math.min(#entries, 5)

  for i = 1, limit do
    local entry = entries[i]
    local marker = entry.name == current_name and "Current" or ""
    table.insert(rows, {
      tostring(i),
      entry.name,
      fmt_duration(entry.seconds),
      marker
    })
  end

  if #rows == 0 then
    table.insert(rows, { "-", "No tracked channels yet", "0s", "" })
  end

  return rows, #entries
end

local function window_spec()
  ensure_initialized()
  local now_local = os.date("*t")
  local tz_name = os.date("%Z")
  local session_seconds = session_seconds_now()
  local current_channel_name = current_channel_label()
  local channel_rows, total_channels = build_channel_rows(current_channel_name)
  local toggle_text = state.show_all_channels and "Show Top Channels" or "Show All Channels"

  return {
    id = "clock_usage",
    title = "Crust Clock",
    open = state.window_open,
    scroll = true,
    default_width = 560,
    default_height = 500,
    min_width = 420,
    min_height = 320,
    children = {
      { type = "heading", text = "Crust Clock" },
      {
        type = "text",
        text = "Track current time, this session, total Crust usage, and focused-channel time.",
        style = { text_role = "muted" }
      },
      {
        type = "group",
        title = "Overview",
        children = {
          {
            type = "list",
            items = {
              { label = "Local time", value = fmt_time(now_local, use_24h()) .. " " .. tz_name },
              { label = "Session time", value = fmt_duration(session_seconds) },
              { label = "Overall time", value = fmt_duration(state.overall_total_seconds) },
              { label = "Focused channel", value = current_channel_name },
              { label = "Tracked channels", value = tostring(total_channels) }
            }
          }
        }
      },
      {
        type = "group",
        title = "Per-Channel Time",
        children = {
          {
            type = "row",
            children = {
              { type = "button", id = "toggle_channels", text = toggle_text, action = "toggle_channels" },
              { type = "button", id = "refresh", text = "Refresh Now", action = "refresh" }
            }
          },
          {
            type = "table",
            columns = {
              { id = "rank", title = "#" },
              { id = "channel", title = "Channel" },
              { id = "time", title = "Time" },
              { id = "current", title = "Current" }
            },
            rows = channel_rows
          }
        }
      },
      {
        type = "text",
        text = "Data file: " .. state_file,
        style = { text_role = "muted", emphasis = "small" }
      }
    }
  }
end

local function render_window()
  ensure_initialized()
  local spec = window_spec()
  if state.registered then
    c2.ui.update_window("clock_usage", spec)
  else
    c2.ui.register_window(spec)
    state.registered = true
  end
end

local function tick()
  if not state.ticking then
    return
  end
  sample_usage()
  if state.window_open then
    render_window()
  end
  if state.window_open then
    c2.later(tick, tick_interval_ms)
  else
    state.ticking = false
  end
end

c2.register_callback(c2.EventType.PluginUiAction, function(ev)
  if ev.surface_id ~= "clock_usage" then
    return
  end

  ensure_initialized()

  if ev.action == "toggle_channels" then
    state.show_all_channels = not state.show_all_channels
  end

  if ev.action == "refresh" then
    sample_usage()
  end

  render_window()
end)

c2.register_callback(c2.EventType.PluginUiWindowClosed, function(ev)
  if ev.window_id ~= "clock_usage" then
    return
  end

  ensure_initialized()
  state.window_open = false
  write_state(state_file, state)
end)

c2.register_command("crusttime", function(ctx)
  ensure_initialized()
  sample_usage()
  state.window_open = true
  render_window()
  if not state.ticking then
    state.ticking = true
    c2.later(tick, tick_interval_ms)
  end
  c2.ui.open_window("clock_usage")
  return "Opened the Crust Clock window."
end, {
  usage = "/crusttime",
  summary = "Open the Crust Clock window",
  aliases = { "usageclock", "clockui" },
})

c2.log(c2.LogLevel.Info, "Crust Clock Plugin loaded")
