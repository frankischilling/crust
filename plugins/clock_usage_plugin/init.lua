local function join_path(base, name)
  if base:sub(-1) == "/" then
    return base .. name
  end
  return base .. "/" .. name
end

local plugin_base_dir = c2.plugin_data_dir() or c2.plugin_dir() or "."
local runtime_file = join_path(plugin_base_dir, "runtime_seconds.txt")
local session_started_unix = math.floor(c2.session_started_ms() / 1000)

local function read_persisted_seconds(path)
  local f = io.open(path, "r")
  if not f then
    return 0
  end

  local raw = f:read("*a") or ""
  f:close()

  local parsed = tonumber(raw)
  if not parsed then
    return 0
  end

  return math.max(0, math.floor(parsed))
end

local function write_persisted_seconds(path, seconds)
  local f = io.open(path, "w")
  if not f then
    return
  end

  f:write(tostring(math.max(0, math.floor(seconds or 0))))
  f:close()
end

local runtime_seconds_base = read_persisted_seconds(runtime_file)

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

local function current_runtime_seconds(now)
  now = now or os.time()
  return runtime_seconds_base + math.max(0, now - session_started_unix)
end

local function build_message(mode, now_local, tz_name, session_seconds, overall_seconds)
  local use_24h = true
  if c2.use_24h_timestamps then
    use_24h = c2.use_24h_timestamps()
  end

  if mode == "session" then
    return "Session uptime: " .. fmt_duration(session_seconds)
  end
  if mode == "overall" or mode == "runtime" or mode == "plugin" then
    return "Overall runtime: " .. fmt_duration(overall_seconds)
  end

  return "Crust time and usage | Local: " .. fmt_time(now_local, use_24h) ..
    " | Timezone: " .. fmt_time(now_local, use_24h) .. " " .. tz_name ..
    " | Session uptime: " .. fmt_duration(session_seconds) ..
    " | Overall runtime: " .. fmt_duration(overall_seconds)
end

local function crusttime(ctx)
  local mode = "all"
  if ctx.words and #ctx.words > 1 then
    mode = string.lower(ctx.words[2])
  end

  local now_local = os.date("*t")
  local tz_name = os.date("%Z")
  local now_unix = os.time()
  local session_seconds = now_unix - session_started_unix
  local overall_seconds = current_runtime_seconds(now_unix)
  write_persisted_seconds(runtime_file, overall_seconds)

  c2.add_system_message(
    ctx.channel_name,
    build_message(mode, now_local, tz_name, session_seconds, overall_seconds)
  )
end

c2.log(c2.LogLevel.Info, "Clock and Usage Plugin loaded")

c2.register_command("crusttime", crusttime, {
  usage = "/crusttime [all|session|overall]",
  summary = "Show current time, session uptime, and overall runtime",
  aliases = { "usageclock", "uptimeclock" },
})
