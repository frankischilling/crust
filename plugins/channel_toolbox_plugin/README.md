# Channel toolbox plugin

Adds `/chatbox` commands for channel lookup, chat messages, local notes, and
clearing the local message buffer. It uses:

- `c2.channel_by_name()`
- `c2.send_message()`
- `c2.add_system_message()`
- `c2.clear_messages()`
- `c2.register_callback()` for command completions

Try these commands:

- `/chatbox info`
- `/chatbox note`
- `/chatbox say hello from Lua`
- `/chatbox clear`
