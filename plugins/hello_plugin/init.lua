local function hello_cmd(ctx)
  local channel_name = ctx.channel_name or ""
  local text = "Hello from Crust plugins"
  if channel_name ~= "" then
    text = text .. " in #" .. channel_name
  end
  c2.add_system_message(ctx.channel_name, text .. "!")
end

c2.log(c2.LogLevel.Info, "Hello Plugin loaded from", c2.plugin_dir())

c2.register_command("hello", hello_cmd, {
  usage = "/hello",
  summary = "Show a local hello message",
  aliases = { "hi" },
})

c2.register_callback(c2.EventType.CompletionRequested, function(ev)
  if ev.query == "hel" then
    return {
      hide_others = true,
      values = { "hello" },
    }
  end
  return { hide_others = false, values = {} }
end)
