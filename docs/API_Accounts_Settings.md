# Accounts And Settings

These helpers update account state, connection identity, and persisted app
preferences.

## Account Management

## `c2.login(token)`

Log in with a Twitch token.

## `c2.logout()`

Log out the active account.

## `c2.add_account(token)`

Add another account using a Twitch token.

## `c2.switch_account(username)`

Switch to a saved account by username.

## `c2.remove_account(username)`

Remove a saved account by username.

## `c2.set_default_account(username)`

Set the default saved account.

## `c2.refresh_auth()`

Trigger a refresh of the current authentication state.

Useful callback events:

- `c2.EventType.Authenticated`
- `c2.EventType.LoggedOut`
- `c2.EventType.AccountListUpdated`
- `c2.EventType.AuthExpired`

## IRC Identity

## `c2.set_irc_nick(nick)`

Set the IRC nickname used by Crust.

## `c2.set_irc_auth(nickserv_user, nickserv_pass)`

Update IRC authentication credentials.

## Feature Flags And Shell Preferences

## `c2.set_beta_features(settings)`

`settings` fields:

- `kick_enabled`
- `irc_enabled`

The corresponding callback event, `c2.EventType.BetaFeaturesUpdated`, also
includes the current IRC NickServ credentials and always-on-top setting because
the runtime emits a broader persisted snapshot than this setter accepts.

## `c2.set_always_on_top(enabled)`

Set the always-on-top preference.

## `c2.set_theme(theme)`

Set the active theme name.

## `c2.set_chat_ui_behavior(settings)`

`settings` fields:

- `prevent_overlong_twitch_messages`
- `collapse_long_messages`
- `collapse_long_message_lines`
- `animations_when_focused`

## `c2.set_general_settings(settings)`

`settings` fields:

- `show_timestamps`
- `show_timestamp_seconds`
- `use_24h_timestamps`
- `local_log_indexing_enabled`
- `auto_join`
- `highlights`
- `ignores`

The matching event also includes:

- `desktop_notifications_enabled`

## `c2.set_slash_usage_counts(settings)`

`settings` is a table containing:

- `usage_counts`

Each `usage_counts` entry includes:

- `name`
- `count`

The Rust parser also accepts `command` as an alternative to `name`, but the
event payload always emits `name`.

## `c2.set_emote_picker_preferences(settings)`

`settings` fields:

- `favorites`
- `recent`
- `provider_boost`

## `c2.set_appearance_settings(settings)`

`settings` fields:

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

## `c2.set_notification_settings(settings)`

`settings` fields:

- `desktop_notifications_enabled`

## Rules And Local Moderation Preferences

## `c2.set_highlight_rules(rules)`

`rules` is an array of rule tables.

Each rule may include:

- `pattern`
- `is_regex`
- `case_sensitive`
- `enabled`
- `show_in_mentions`
- `color`
- `has_alert`
- `has_sound`
- `sound_url`

When present, `color` is a three-item RGB array:

- `color[1]`: red
- `color[2]`: green
- `color[3]`: blue

## `c2.set_filter_records(records)`

`records` is an array of filter tables.

Each record may include:

- `name`
- `pattern`
- `is_regex`
- `case_sensitive`
- `enabled`
- `channel`
- `scope`
- `action`
- `filter_sender`

`action` currently maps to:

- `Hide`
- `Dim`

`scope` currently maps to:

- `Global`
- `Channel`

When `scope` is `Channel`, the emitted callback payload also includes the
resolved `channel` snapshot table.

## `c2.set_mod_action_presets(presets)`

`presets` is an array of preset tables.

Each preset may include:

- `label`
- `command_template`
- `icon_url`

`icon_url` is omitted in callback payloads when not set.

## Matching Events

Most settings helpers eventually produce one of these callback payloads:

- `c2.EventType.BetaFeaturesUpdated`
- `c2.EventType.ChatUiBehaviorUpdated`
- `c2.EventType.GeneralSettingsUpdated`
- `c2.EventType.SlashUsageCountsUpdated`
- `c2.EventType.EmotePickerPreferencesUpdated`
- `c2.EventType.AppearanceSettingsUpdated`
- `c2.EventType.HighlightRulesUpdated`
- `c2.EventType.FilterRecordsUpdated`
- `c2.EventType.ModActionPresetsUpdated`

## Example

```lua
local account = c2.current_account()
c2.log(c2.LogLevel.Info, "active account: " .. tostring(account.username or ""))

c2.set_notification_settings({
  desktop_notifications_enabled = true,
})
```
