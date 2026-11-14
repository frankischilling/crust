# Chat and channels

These helpers send messages, navigate channels, and request data. Fetch results
arrive through event callbacks.

## Message helpers

## `c2.send_message(channel, text, opts?)`

Send a chat message.

- `channel`: channel name string or channel table
- `text`: message text
- `opts`: optional table

`opts` may include:

- `reply_to_msg_id`
- `reply`

`opts.reply` may include:

- `parent_msg_id`
- `parent_user_login`
- `parent_display_name`
- `parent_msg_body`

`reply_to_msg_id` is forwarded to the host as the server-assigned parent
message id. `reply` is optional local-echo/context metadata used to describe
the replied-to message in Lua and the UI.

The two-argument form, `c2.send_message(channel, text)`, is also supported.
Malformed `opts` fields are ignored. A malformed `channel` or `text` prevents
the send.

## `c2.send_whisper(target_login, text)`

Send a Twitch whisper.

- `target_login`: Twitch login
- `text`: whisper body

## `c2.add_system_message(channel, text)`

Add a local system message to a channel feed. The message appears only in
Crust and is not sent to the chat service.

- `channel`: channel name string or channel table
- `text`: local message body

## `c2.clear_messages(channel)`

Clear a channel's visible message buffer in Crust. This affects only the local
chat view.

## `c2.clear_user_messages_locally(channel, login)`

Remove one user's visible messages from the local chat view.

- `channel`: channel name string or channel table
- `login`: user login to clear locally

The matching callback event is `c2.EventType.ClearUserMessagesLocally`.

## Channel actions

## `c2.join_channel(channel)`

Join a Twitch, Kick, or IRC channel handled by Crust.

## `c2.join_irc_channel(channel, key?)`

Join a raw IRC channel.

- `channel`: IRC channel name or channel table
- `key`: optional IRC channel key

## `c2.leave_channel(channel)`

Part a channel.

## `c2.show_user_card(login, channel)`

Open the user card for `login` in the given channel context.

## `c2.open_url(url)`

Open a URL in the system browser.

## Fetch helpers

These calls start a fetch. The result arrives in a later callback.

## `c2.fetch_image(url)`

Fetch an image. The matching callback is usually:

- `c2.EventType.EmoteImageReady`

`EmoteImageReady.raw_bytes_base64` is a base64 string containing the fetched
image bytes. No raw Lua byte-array table is emitted.

## `c2.fetch_link_preview(url)`

Fetch link metadata. The matching callback is:

- `c2.EventType.LinkPreviewReady`

## `c2.load_channel_emotes(channel_twitch_id)`

Fetch the emote set for a Twitch channel.

- `channel_twitch_id`: Twitch numeric user id string

The matching callback is:

- `c2.EventType.ChannelEmotesLoaded`

## `c2.fetch_stream_status(login)`

Fetch current stream status for a Twitch login.

The matching callback is:

- `c2.EventType.StreamStatusUpdated`

## `c2.fetch_user_profile(login)`

Fetch a user profile.

The matching callbacks are:

- `c2.EventType.UserProfileLoaded`
- `c2.EventType.UserProfileUnavailable`

## `c2.fetch_ivr_logs(channel, username)`

Fetch IVR logs for a channel and username.

- `channel`: channel login string
- `username`: user login string

The matching callbacks are:

- `c2.EventType.IvrLogsLoaded`
- `c2.EventType.IvrLogsFailed`

## `c2.load_older_local_history(channel, before_ts_ms, limit, before_row_id?)`

Load older locally indexed messages for a channel.

- `channel`: channel name string or channel table
- `before_ts_ms`: history cutoff in Unix milliseconds
- `limit`: maximum number of messages to request, capped at 5,000
- `before_row_id`: optional SQLite row cursor from the previous `HistoryLoaded` event

When `before_row_id` is omitted, the cutoff timestamp is exclusive. For later
pages, pass both `next_ts_ms` and `next_row_id` from the previous event. The pair
selects rows strictly before that cursor, including
other messages with the same millisecond timestamp. Stop when `has_more` is
`false`. A page can be empty when saved rows are unreadable; use its cursor and
`has_more` value to decide whether to continue.

This helper reads locally indexed history. It does not fetch remote history
from Twitch.

The matching callback is:

- `c2.EventType.HistoryLoaded`

## Example

```lua
local channel = c2.channel_by_name("some_channel")

c2.register_callback(c2.EventType.LinkPreviewReady, function(ev)
  c2.log(c2.LogLevel.Info, "preview for " .. tostring(ev.url or ""))
end)

c2.send_message(channel, "Hello from a plugin")
c2.send_message(channel, "Reply from a plugin", {
  reply_to_msg_id = "server-parent-id",
  reply = {
    parent_msg_id = "server-parent-id",
    parent_user_login = "some_user",
    parent_display_name = "SomeUser",
    parent_msg_body = "Original message text",
  },
})
c2.show_user_card("some_user", channel)
c2.fetch_link_preview("https://github.com/frankischilling/crust")
```
