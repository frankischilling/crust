# Moderation And Creator Tools

These helpers trigger moderation actions, creator tools, and live-control
operations.

## Moderation Actions

## `c2.timeout_user(channel, login, user_id, seconds, reason?)`

Timeout a user in a channel.

## `c2.ban_user(channel, login, user_id, reason?)`

Ban a user in a channel.

## `c2.unban_user(channel, login, user_id)`

Unban a user in a channel.

## `c2.warn_user(channel, login, user_id, reason)`

Issue a warning to a user.

## `c2.set_suspicious_user(channel, login, user_id, restricted)`

Mark or unmark a user as suspicious.

- `restricted`: boolean flag passed through to the host

## `c2.clear_suspicious_user(channel, login, user_id)`

Clear suspicious-user state.

## `c2.resolve_automod_message(channel, message_id, sender_user_id, action)`

Resolve an AutoMod queue item.

- `action` is passed through as the host action string, typically `ALLOW` or
  `DENY`
- when `action` cannot be parsed as a string, the current Lua bridge falls back
  to `ALLOW`

## `c2.fetch_unban_requests(channel)`

Fetch unban requests for a channel.

Matching events:

- `c2.EventType.UnbanRequestsLoaded`
- `c2.EventType.UnbanRequestsFailed`

## `c2.resolve_unban_request(channel, request_id, approve, resolution_text?)`

Resolve an unban request.

- `approve`: boolean
- `resolution_text`: optional moderator response

## `c2.open_moderation_tools(channel?)`

Open the moderation tools view. `channel` is optional.

Matching event:

- `c2.EventType.OpenModerationTools`

## Reward Redemptions

## `c2.update_reward_redemption_status(channel, reward_id, redemption_id, status, user_login, reward_title)`

Update a channel-points reward redemption.

- `status` should be `FULFILLED` or `CANCELED`
- `reward_id`: Twitch reward identifier
- `redemption_id`: Twitch redemption identifier
- `user_login`: login used for status messages and moderation context
- `reward_title`: human-readable reward title

## Message Cleanup

## `c2.delete_message(channel, message_id)`

Delete a message by id.

Matching event:

- `c2.EventType.MessageDeleted`

## `c2.clear_user_messages_locally(channel, login)`

Clear one user's visible messages from the local chat view.

Matching events:

- `c2.EventType.ClearUserMessagesLocally`

`c2.clear_user_messages_locally(...)` triggers a local-only hide/clear request.
`c2.EventType.UserMessagesCleared` is different: it reports a moderation-driven
clear from upstream runtime state.

## Creator Tools

## `c2.create_poll(channel, title, choices, duration_secs, channel_points_per_vote?)`

Create a poll.

- `choices`: array of choice strings
- `channel_points_per_vote`: optional channel-points price

## `c2.end_poll(channel, status)`

End the active poll.

- `status` is forwarded to the host as the final poll state

## `c2.create_prediction(channel, title, outcomes, duration_secs)`

Create a prediction.

- `outcomes`: array of outcome strings

## `c2.lock_prediction(channel)`

Lock the active prediction.

## `c2.resolve_prediction(channel, winning_outcome_index)`

Resolve the active prediction.

- `winning_outcome_index`: one-based winning outcome index

## `c2.cancel_prediction(channel)`

Cancel the active prediction.

## `c2.start_commercial(channel, length_secs)`

Start a commercial break.

## `c2.create_stream_marker(channel, description?)`

Create a stream marker.

## `c2.send_announcement(channel, message, color?)`

Send a channel announcement.

- `color` is optional and passed through as the platform color string

## `c2.send_shoutout(channel, target_login)`

Send a shoutout to another channel.

## Example

```lua
local channel = c2.channel_by_name("some_channel")

c2.open_moderation_tools(channel)
c2.send_announcement(channel, "Moderation tools opened from Lua", "primary")
```
