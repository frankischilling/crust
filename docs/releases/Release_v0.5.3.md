# Crust v0.5.3

This patch keeps chat usable during startup, reduces image downloads for hidden
content, and adds live channel-emote updates. It also separates saved whispers
by account and fixes chat navigation and focus.

## Startup and images

- Chat and settings open immediately. A compact, dismissible status strip shows
  startup progress while channels, history, and emote catalogs load. It clears
  readiness if the connection drops.
- Emote providers publish their catalogs as each response arrives. A slow
  provider no longer holds back the others.
- The Twitch picker and autocomplete load the signed-in account's paginated emote
  inventory. Add the optional `user:read:emotes` token scope to use it; chat still
  works without that scope.
- Emotes, badges, and avatars load when displayed. Startup no longer downloads
  every image in every catalog, and hidden conversations and feed entries no
  longer queue their images.
- User profiles and shared-chat metadata appear without waiting for avatars.
- The live feed renders the rows in view instead of every followed channel.
- Room-mode updates keep their UI state current without repeatedly fetching
  local history, remote history, or pinned snapshots. Badge loads coalesce,
  retry after failures, and refresh when the account or room changes.
- Badge-cache saves no longer block the chat event loop.
- Selected 7TV username paints reach chat and recent mentions. Removing a paint
  clears it, while failed or obsolete responses preserve the current style.
- Joined Twitch channels receive live BTTV and 7TV additions, renames, and
  removals. 7TV also supports joined Kick channels and active-set changes.
  Reconnect snapshots preserve updates received during loading, and obsolete
  subscriptions cannot replace the current set.

## Private messages and connections

- Saved whispers belong to the account's immutable Twitch user ID. Renaming the
  account keeps its history; a different account that reuses the login cannot
  read it. Switching accounts cancels the old replay, and stale private events
  are rejected before reaching the UI or plugin callbacks.
- Delayed whisper send failures stay with the account that sent the message.
  They appear as temporary notices that clear on account changes or logout,
  without entering channel history.
- Older whisper rows with no owner or only a login-based owner remain in the
  database without being assigned to a current account.
- IRC private messages open a conversation with the sender. Replies and actions
  address that nickname, and private tabs remain separate from rooms with the
  same name. Failed sends do not create a new conversation tab.
- Account changes and shutdown cancel a stalled initial EventSub connection.
- A delayed avatar response cannot replace the active account's avatar after a
  switch or logout.
- Whispers preserve the sender's user ID through live delivery and saved history,
  so personal emotes resolve for the right sender. Account-owned history stays
  readable when a sender ID is missing.
- Accounts with localized display names use their validated login for whisper
  routing and history.
- Message parsing preserves Unicode whitespace and character boundaries and
  ignores malformed native-emote ranges whose end precedes their start.

## Navigation and dialogs

- Quick switch handles keys while its search has focus. Keyboard selection
  scrolls into view, and a stationary pointer no longer overrides it. The
  palette fits narrow windows.
- Closing search restores the full chat. Search fields keep focus while typing
  and resizing instead of repeatedly returning to the keyword field.
- Search header actions wrap in narrow windows without covering the title or
  close control.
- Duplicate channel panes retain independent scrolling and pause state.
- Whisper Reply targets the visible composer and preserves an existing draft.
  A self-sent whisper does not create an incoming-message notification.
- Join and login fields request focus once when opened. Tab can leave an empty
  field, and Enter and Escape act on the dialog that owns focus.
- Shortcut rebinding captures the key before app shortcuts can use it. Leaving
  Hotkeys or closing Settings cancels an unfinished capture.
- Layout buttons show the active layout. Tabs selects top tabs; Sidebar is
  disabled with an explanation when the window is too narrow.
- Icon buttons show keyboard focus and supply labels to accessibility tools.
- Expired-token guidance points to the account button in the top bar and
  Add Account.

BTTV's cached snapshot service can delay updates missed while disconnected.
Global and personal sets retain their existing refresh paths.

Channel-specific Twitch follower emotes and live updates for global and personal
sets remain follow-ups in
[issue #9](https://github.com/frankischilling/crust/issues/9).
