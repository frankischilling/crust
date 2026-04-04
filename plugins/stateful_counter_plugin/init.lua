local function join_path(base, name)
  if base:sub(-1) == "/" then
    return base .. name
  end
  return base .. "/" .. name
end

local function read_state(path)
  local f = io.open(path, "r")
  if not f then
    return 0, ""
  end

  local raw = f:read("*a") or ""
  f:close()

  local count, note = raw:match("^(%-?%d+)%s*(.*)$")
  return math.max(0, tonumber(count) or 0), note or ""
end

local function write_state(path, count, note)
  local f = io.open(path, "w")
  if not f then
    return false
  end

  f:write(tostring(math.max(0, math.floor(count or 0))))
  f:write("\t")
  f:write(tostring(note or ""))
  f:close()
  return true
end

local plugin_dir = c2.plugin_data_dir() or c2.plugin_dir() or "."
local state_file = join_path(plugin_dir, "counter_state.txt")
local counter_value, counter_note = read_state(state_file)

local function fmt_mode_list()
  return "show, inc, reset, note"
end

local function complete_counter(ev)
  if not ev.is_first_word then
    return { hide_others = false, values = {} }
  end

  local query = string.lower(ev.query or "")
  if query == "c" or query == "co" or query == "cou" then
    return {
      hide_others = true,
      values = { "counter", "counter show", "counter inc", "counter reset", "counter note" },
    }
  end

  return { hide_others = false, values = {} }
end

local function counter_cmd(ctx)
  local mode = "show"
  if ctx.words and #ctx.words > 1 then
    mode = string.lower(ctx.words[2])
  end

  if mode == "inc" or mode == "add" then
    counter_value = counter_value + 1
    write_state(state_file, counter_value, counter_note)
    c2.add_system_message(
      ctx.channel_name,
      "Counter incremented to " .. tostring(counter_value) .. " and saved to " .. state_file
    )
    return
  end

  if mode == "reset" then
    counter_value = 0
    counter_note = ""
    write_state(state_file, counter_value, counter_note)
    c2.add_system_message(ctx.channel_name, "Counter reset to zero")
    return
  end

  if mode == "note" then
    local note = table.concat(ctx.words, " ", 3)
    if note == "" then
      note = "Saved note from stateful_counter_plugin"
    end
    counter_note = note
    write_state(state_file, counter_value, counter_note)
    c2.add_system_message(ctx.channel_name, "Saved note: " .. counter_note)
    return
  end

  c2.add_system_message(
    ctx.channel_name,
    "Counter: " .. tostring(counter_value) ..
      " | Note: " .. (counter_note ~= "" and counter_note or "(none)") ..
      " | Data file: " .. state_file ..
      " | Modes: " .. fmt_mode_list()
  )
end

c2.log(c2.LogLevel.Info, "Stateful Counter Plugin loaded from " .. tostring(c2.plugin_dir()))

c2.register_callback(c2.EventType.CompletionRequested, complete_counter)

c2.register_command("counter", counter_cmd, {
  usage = "/counter [show|inc|reset|note <text>]",
  summary = "Demonstrate plugin_data_dir persistence",
  aliases = { "count", "state" },
})
