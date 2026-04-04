# Event Callback Demo Plugin

This example focuses on `c2.register_callback()` and the async fetch/event flow.

It shows:

- `c2.register_callback()`
- `c2.fetch_user_profile()`
- `c2.fetch_link_preview()`
- `c2.fetch_stream_status()`
- maintaining lightweight Lua-side state from callbacks

Try:

- `/eventdemo`
- `/eventdemo profile some_login`
- `/eventdemo preview https://github.com/frankischilling/crust`
- `/eventdemo stream some_login`
