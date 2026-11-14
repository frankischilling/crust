# Event callback demo

Requests profiles, link previews, and stream status, then handles the responses
in Lua callbacks. The example uses:

- `c2.register_callback()`
- `c2.fetch_user_profile()`
- `c2.fetch_link_preview()`
- `c2.fetch_stream_status()`

Try these commands:

- `/eventdemo`
- `/eventdemo profile some_login`
- `/eventdemo preview https://github.com/frankischilling/crust`
- `/eventdemo stream some_login`
