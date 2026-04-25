# Events

Callbacks registered with `c2.register_callback()` receive a typed Lua table.
Every payload includes a `type` field matching the event name.

## Common Nested Tables

Some events include nested tables reused across the API:

- `channel`: same shape as `c2.channel_by_name()`
- `badge`: badge snapshot table
- `name_paint`: sender name-paint table
- `profile`: user profile snapshot table
- `message`: chat message table
- `sender`: message sender table
- `flags`: message flags table
- `reply`: reply metadata table
- `twitch_emote`: Twitch emote range table
- `notice`: system notice table
- `item`: AutoMod queue item table
- `request`: unban-request table
- `ivr_log_entry`: IVR log entry table

The shared `channel` table includes:

- `name`
- `display_name`
- `platform`: one of `twitch`, `kick`, or `irc`
- `id`
- `is_twitch`
- `is_irc`
- `is_kick`
- `is_joined`: omitted only when the host has no channel snapshot
- `is_mod`: omitted only when the host has no channel snapshot
- `is_vip`: omitted only when the host has no channel snapshot
- `is_broadcaster`: omitted only when the host has no channel snapshot

The shared `badge` table includes:

- `name`
- `version`
- `url`: omitted when the badge has no image URL

The shared `name_paint` table includes:

- `function`
- `angle`: omitted when the paint has no angle
- `repeat`
- `image_url`: omitted when the paint has no backing image URL
- `shadows`: always present, possibly empty
- `stops`: always present, possibly empty

Each `shadows` entry includes:

- `x_offset`
- `y_offset`
- `radius`
- `color`

Each `stops` entry includes:

- `at`: integer stop position in thousandths
- `color`

The shared `message` table includes:

- `id`
- `server_id`: omitted when the message has no server-assigned id
- `timestamp`
- `channel`
- `sender`
- `raw_text`
- `spans`: always present, possibly empty
- `twitch_emotes`: always present, possibly empty
- `flags`
- `reply`: omitted when the message is not a reply
- `msg_kind`

The shared `sender` table includes:

- `user_id`
- `login`
- `display_name`
- `color`: omitted when the sender has no color
- `name_paint`: omitted when the sender has no resolved name paint
- `badges`: always present, possibly empty

The shared `flags` table includes:

- `is_action`
- `is_highlighted`
- `is_deleted`
- `is_first_msg`
- `is_pinned`
- `is_self`
- `is_mention`
- `custom_reward_id`: omitted when the message is not tied to a custom reward
- `is_history`

The shared `reply` table includes:

- `parent_msg_id`
- `parent_user_login`
- `parent_display_name`
- `parent_msg_body`

The shared `twitch_emote` table includes:

- `id`
- `start`
- `end`

The shared `profile` table includes:

- `id`
- `login`
- `display_name`
- `description`
- `created_at`: omitted when unavailable
- `avatar_url`: omitted when unavailable
- `followers`: omitted when unavailable
- `is_partner`
- `is_affiliate`
- `pronouns`: omitted when unavailable
- `followed_at`: omitted when unavailable
- `chat_color`: omitted when unavailable
- `is_live`
- `stream_title`: omitted when unavailable
- `stream_game`: omitted when unavailable
- `stream_viewers`: omitted when unavailable
- `last_broadcast_at`: omitted when unavailable
- `is_banned`
- `ban_reason`: omitted when unavailable

The shared `notice` table includes:

- `channel`: omitted when the notice is not tied to a specific channel
- `text`
- `timestamp`

The shared `item` table includes:

- `message_id`
- `sender_user_id`
- `sender_login`
- `text`
- `reason`: omitted when the queue item has no moderation reason

The shared `request` table includes:

- `request_id`
- `user_id`
- `user_login`
- `text`: omitted when unavailable
- `created_at`: omitted when unavailable
- `status`: omitted when unavailable

The shared `ivr_log_entry` table includes:

- `text`
- `timestamp`
- `display_name`
- `msg_type`

## Completion

### `c2.EventType.CompletionRequested`

Fields:

- `query`
- `full_text_content`
- `cursor_position`
- `is_first_word`
- `channel`: omitted when completion is requested without channel context

Return a table with:

- `values`
- `hide_others`

## Account And Connection Events

### `c2.EventType.Authenticated`

Fields:

- `username`
- `user_id`

### `c2.EventType.LoggedOut`

No payload fields.

### `c2.EventType.AccountListUpdated`

Fields:

- `accounts`
- `active`: omitted when no authenticated account is active
- `default`: omitted when no default account is configured

### `c2.EventType.ConnectionStateChanged`

Fields:

- `state`

`state` is currently rendered to one of these strings:

- `Disconnected`
- `Connecting...`
- `Connected`
- `Reconnecting (attempt N)...`
- `Error: ...`

### `c2.EventType.AuthExpired`

No payload fields.

## Loader And Emote Events

### `c2.EventType.EmoteImageReady`

Fields:

- `uri`
- `width`
- `height`
- `raw_bytes_base64`

`raw_bytes_base64` contains the image bytes encoded as base64.
No Lua numeric byte array is emitted.

### `c2.EventType.EmoteCatalogUpdated`

Fields:

- `emotes`

Each `emotes` entry includes:

- `code`
- `provider`
- `url`
- `scope`

### `c2.EventType.ImagePrefetchQueued`

Fields:

- `count`

This event reports how many image-prefetch jobs were queued.

## Chat And Channel Events

### `c2.EventType.ChannelJoined`

Fields:

- `channel`

### `c2.EventType.ChannelParted`

Fields:

- `channel`

### `c2.EventType.ChannelRedirected`

Fields:

- `old_channel`
- `new_channel`

### `c2.EventType.ChannelMessagesCleared`

Fields:

- `channel`

This event reports that the runtime cleared the whole visible channel buffer.

### `c2.EventType.ClearUserMessagesLocally`

Fields:

- `channel`

This event reports a local-only request to hide one user's messages in the
current channel view.

### `c2.EventType.MessageReceived`

Fields:

- `channel`
- `message`

The `message` table includes the same rich message metadata the UI uses,
including sender data, spans, flags, timestamps, reply info, and message kind.

`message`, `sender`, `flags`, and `reply` use the shared table shapes
documented above.

Each `spans` entry includes a `type` field and then type-specific fields:

- `Text`: `text`, `is_action`
- `Emote`: `id`, `code`, `url`, `url_hd`, `provider`
- `Emoji`: `text`, `url`
- `Badge`: `name`, `version`
- `Mention`: `login`
- `Url`: `text`, `url`

For `Emote`, `url_hd` is omitted when no HD image URL exists.

The nested `msg_kind` table includes a `type` field and then variant-specific fields:

- `Chat`
- `Sub`: `display_name`, `months`, `plan`, `is_gift`, `sub_msg`
- `Raid`: `display_name`, `viewer_count`
- `Timeout`: `login`, `seconds`
- `Ban`: `login`
- `ChatCleared`
- `SystemInfo`
- `ChannelPointsReward`: `user_login`, `reward_title`, `cost`, `reward_id`, `redemption_id`, `user_input`, `status`
- `SuspiciousUserMessage`
- `Bits`: `amount`

For `ChannelPointsReward`, `reward_id`, `redemption_id`, `user_input`, and
`status` are omitted when unavailable.

### `c2.EventType.WhisperReceived`

Fields:

- `from_login`
- `from_display_name`
- `target_login`
- `text`
- `twitch_emotes`
- `is_self`
- `timestamp`
- `is_history`

Each `twitch_emotes` entry includes:

- `id`
- `start`
- `end`

### `c2.EventType.MessageDeleted`

Fields:

- `channel`
- `server_id`

### `c2.EventType.UserMessagesCleared`

Fields:

- `channel`
- `login`

This event reports a moderation-driven user clear from the runtime.

### `c2.EventType.SystemNotice`

Fields:

- `notice`

The nested `notice` table includes:

- `channel`
- `text`
- `timestamp`

### `c2.EventType.Error`

Fields:

- `context`
- `message`

### `c2.EventType.IrcTopicChanged`

Fields:

- `channel`
- `topic`

### `c2.EventType.UserStateUpdated`

Fields:

- `channel`
- `is_mod`
- `badges`
- `color`: omitted when the runtime has no user color

Each `badges` entry includes:

- `name`
- `version`
- `url`

Each badge entry uses the shared `badge` table shape, so `url` is omitted when
the badge has no image URL.

### `c2.EventType.SelfAvatarLoaded`

Fields:

- `avatar_url`

### `c2.EventType.SenderCosmeticsUpdated`

Fields:

- `user_id`
- `color`: omitted when no color update is available
- `name_paint`: omitted when no name-paint update is available
- `badge`: omitted when no badge update is available
- `avatar_url`: omitted when no avatar update is available

When present, `name_paint` includes:

- `function`
- `angle`
- `repeat`
- `image_url`
- `shadows`
- `stops`

Each `shadows` entry includes:

- `x_offset`
- `y_offset`
- `radius`
- `color`

Each `stops` entry includes:

- `at`
- `color`

When present, `badge` uses the shared `badge` table shape.

## Fetch Result Events

### `c2.EventType.HistoryLoaded`

Fields:

- `channel`
- `messages`

Each `messages` entry uses the same `message` table shape documented under
`c2.EventType.MessageReceived`.

### `c2.EventType.UserProfileLoaded`

Fields:

- `profile`

`profile` uses the shared profile table shape documented above, including the
same omission rules for optional fields.

### `c2.EventType.UserProfileUnavailable`

Fields:

- `login`

### `c2.EventType.StreamStatusUpdated`

Fields:

- `login`
- `is_live`
- `title`: omitted when unavailable
- `game`: omitted when unavailable
- `viewers`: omitted when unavailable

### `c2.EventType.IvrLogsLoaded`

Fields:

- `username`
- `messages`

Each `messages` entry includes:

- `text`
- `timestamp`
- `display_name`
- `msg_type`

`msg_type` is currently:

- `1`: normal message
- `2`: timeout/ban event

### `c2.EventType.IvrLogsFailed`

Fields:

- `username`
- `error`

### `c2.EventType.ChannelEmotesLoaded`

Fields:

- `channel`
- `count`

### `c2.EventType.LinkPreviewReady`

Fields:

- `url`
- `title`
- `description`
- `thumbnail_url`
- `site_name`

These fields are omitted when the upstream metadata source does not provide
them.

## Settings And Moderation Events

### `c2.EventType.BetaFeaturesUpdated`

Fields:

- `kick_enabled`
- `irc_enabled`
- `irc_nickserv_user`
- `irc_nickserv_pass`
- `always_on_top`

### `c2.EventType.ChatUiBehaviorUpdated`

Fields:

- `prevent_overlong_twitch_messages`
- `collapse_long_messages`
- `collapse_long_message_lines`
- `animations_when_focused`

### `c2.EventType.GeneralSettingsUpdated`

Fields:

- `show_timestamps`
- `show_timestamp_seconds`
- `use_24h_timestamps`
- `local_log_indexing_enabled`
- `auto_join`
- `highlights`
- `ignores`
- `desktop_notifications_enabled`

### `c2.EventType.SlashUsageCountsUpdated`

Fields:

- `usage_counts`

Each `usage_counts` entry includes:

- `name`
- `count`

### `c2.EventType.EmotePickerPreferencesUpdated`

Fields:

- `favorites`
- `recent`
- `provider_boost`

### `c2.EventType.AppearanceSettingsUpdated`

Fields:

- `channel_layout`
- `sidebar_visible`
- `analytics_visible`
- `irc_status_visible`
- `tab_style`
- `show_tab_close_buttons`
- `show_tab_live_indicators`
- `split_header_show_title`
- `split_header_show_game`
- `split_header_show_viewer_count`

### `c2.EventType.RoomStateUpdated`

Fields:

- `channel`
- `emote_only`: omitted when unchanged or unavailable
- `followers_only`: omitted when unchanged or unavailable
- `slow`: omitted when unchanged or unavailable
- `subs_only`: omitted when unchanged or unavailable
- `r9k`: omitted when unchanged or unavailable

### `c2.EventType.AutoModQueueAppend`

Fields:

- `channel`
- `item`

The nested `item` table includes:

- `message_id`
- `sender_user_id`
- `sender_login`
- `text`
- `reason`

### `c2.EventType.AutoModQueueRemove`

Fields:

- `channel`
- `message_id`
- `action`

`action` is omitted when the runtime removes the queue item without reporting a
resolution string.

### `c2.EventType.UnbanRequestsLoaded`

Fields:

- `channel`
- `requests`

Each `requests` entry includes:

- `request_id`
- `user_id`
- `user_login`
- `text`
- `created_at`
- `status`

### `c2.EventType.UnbanRequestsFailed`

Fields:

- `channel`
- `error`

### `c2.EventType.UnbanRequestUpsert`

Fields:

- `channel`
- `request`

The nested `request` table includes:

- `request_id`
- `user_id`
- `user_login`
- `text`
- `created_at`
- `status`

### `c2.EventType.UnbanRequestResolved`

Fields:

- `channel`
- `request_id`
- `status`

### `c2.EventType.OpenModerationTools`

Fields:

- `channel`

`channel` may be omitted when the host opens the moderation tools without a
specific channel target.

### `c2.EventType.HighlightRulesUpdated`

Fields:

- `rules`

Each `rules` entry includes:

- `pattern`
- `is_regex`
- `case_sensitive`
- `enabled`
- `show_in_mentions`
- `color`
- `has_alert`
- `has_sound`
- `sound_url`

When present, `color` is a three-item RGB array.
`sound_url` is omitted when unset.

### `c2.EventType.FilterRecordsUpdated`

Fields:

- `records`

Each `records` entry includes:

- `name`
- `pattern`
- `is_regex`
- `case_sensitive`
- `enabled`
- `scope`
- `channel`
- `action`
- `filter_sender`

`scope` is currently one of:

- `Global`
- `Channel`

`action` is currently one of:

- `Hide`
- `Dim`

`channel` is omitted when `scope` is `Global`.

### `c2.EventType.ModActionPresetsUpdated`

Fields:

- `presets`

Each `presets` entry includes:

- `label`
- `command_template`
- `icon_url`

`icon_url` is omitted when unset.

Each `presets` entry includes:

- `label`
- `command_template`
- `icon_url`

`icon_url` is omitted when unset.

### `c2.EventType.PluginUiAction`

Fields:

- `plugin_name`
- `surface_kind`
- `surface_id`
- `widget_id`
- `action`
- `value`
- `form_values`

Notes:

- plugin UI events are dispatched only to the owning plugin
- `surface_kind` is currently `window`, `settings_page`, or `host_panel`.
- `surface_id` matches the registered window id, settings-page id, or host-panel id.
- `action` is omitted when the widget did not declare one.
- `value` is omitted when the action widget did not send one.
- `form_values` is always a table; it may be empty.

### `c2.EventType.PluginUiChange`

Fields:

- `plugin_name`
- `surface_kind`
- `surface_id`
- `widget_id`
- `value`
- `form_values`

Notes:

- plugin UI events are dispatched only to the owning plugin
- `surface_kind` is currently `window`, `settings_page`, or `host_panel`.
- `surface_id` matches the registered window id, settings-page id, or host-panel id.
- `value` is the changed widget value.
- `value` is emitted as a plain Lua string, boolean, number, or array of strings.
- `form_values` is the current host-form snapshot for the surface.
- controlled widgets still emit `value`, but they only appear in `form_values` when `host_form = true`.

### `c2.EventType.PluginUiSubmit`

Fields:

- `plugin_name`
- `surface_kind`
- `surface_id`
- `widget_id`
- `action`
- `form_values`

Notes:

- plugin UI events are dispatched only to the owning plugin
- `surface_kind` is currently `window`, `settings_page`, or `host_panel`.
- `surface_id` matches the registered window id, settings-page id, or host-panel id.
- `widget_id` is omitted when the submit originated from a surface-level action without a widget id.
- `action` is omitted when the submit widget did not declare one.
- `form_values` is the current host-form snapshot for the surface.

### `c2.EventType.PluginUiWindowClosed`

Fields:

- `plugin_name`
- `window_id`

Notes:

- this event is emitted when the user closes a floating plugin window through the window chrome
- calling `c2.ui.close_window(id)` updates retained state but does not emit `PluginUiWindowClosed`

## Subsystem Settings Events

### `c2.EventType.SoundSettingsUpdated`

Full snapshot of per-event sound notification settings.

Fields:

- `events`: table keyed by event name (`mention`, `whisper`, `subscribe`, `raid`, `custom_highlight`). Each value has:
  - `enabled`: boolean
  - `path`: file path (empty string = built-in default ping)
  - `volume`: linear `[0.0, 1.0]`

Notes:

- emitted once at startup with the persisted settings and after every successful `c2.set_sound_settings` call
- missing events in the snapshot fall back to host defaults

### `c2.EventType.HotkeyBindingsUpdated`

Full snapshot of rebindable hotkeys.

Fields:

- `bindings`: table keyed by action name (`zoom_in`, `open_quick_switcher`, `next_tab`, ...). Each value has:
  - `ctrl`, `shift`, `alt`, `command`: booleans
  - `key`: stable key name (`"K"`, `"Tab"`, `"PageUp"`, ...). Empty = unbound.

Notes:

- emitted once at startup (persisted bindings, or defaults) and after every successful `c2.set_hotkey_bindings` call

### `c2.EventType.UploadStarted`

Fields:

- `channel`: channel snapshot table (the channel whose input initiated the upload)

### `c2.EventType.UploadFinished`

Fields:

- `channel`: channel snapshot table
- `ok`: boolean
- `url`: resolved image URL (only when `ok == true`)
- `error`: error message (only when `ok == false`)

## Example

```lua
c2.register_callback(c2.EventType.ImagePrefetchQueued, function(ev)
  c2.log(c2.LogLevel.Info, "queued " .. tostring(ev.count or 0) .. " image jobs")
end)

c2.register_callback(c2.EventType.EmoteImageReady, function(ev)
  c2.log(
    c2.LogLevel.Info,
    "image ready " .. tostring(ev.uri or "") .. " bytes=" .. tostring(#(ev.raw_bytes_base64 or ""))
  )
end)

c2.register_callback(c2.EventType.PluginUiSubmit, function(ev)
  c2.log(c2.LogLevel.Info, "saved ui surface " .. tostring(ev.surface_id or ""))
end)
```
