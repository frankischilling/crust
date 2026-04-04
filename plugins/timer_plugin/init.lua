local function timer_cmd(ctx)
  c2.later(function()
    c2.add_system_message(ctx.channel_name, "Timer fired after 5 seconds")
  end, 5000)
end

c2.log(c2.LogLevel.Info, "Timer Plugin loaded")

c2.register_command("timer", timer_cmd, {
  usage = "/timer",
  summary = "Send a delayed local message",
})
