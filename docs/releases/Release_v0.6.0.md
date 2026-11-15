# Crust v0.6.0

Chat panes can now split both horizontally and vertically. Each view keeps its
own search and filter selection, and recently closed panes can be reopened with
their draft and position. Reply threads have their own composer, and search
results and user-card history display full chat messages with emotes and badges.

## Chat layouts

- Split right with Ctrl+Alt+R or below with Ctrl+Alt+D. Nested layouts support up
  to 32 panes, with the same actions available from the chat header menu.
- Ctrl+Shift+T reopens the most recently closed pane. Reopening restores its
  draft, search, filters, and previous split position when the sibling still exists.
- Pane filters can follow all enabled saved rules, use a subset, or use none.
  Duplicate views of one channel can use different rules and queries.
- Divider positions, focus, search fields, and filter selections survive a
  restart. Older flat layouts migrate automatically, and unavailable channels
  no longer discard the settings of the surviving view.
- Dragged dividers stay at the pointer when it stops moving. Settings writes are
  debounced while typing, with the final edit retained during shutdown.
- Changing a view's channel keeps its unsent draft and reply context for the
  session. Returning to the channel restores that draft.

## Reply threads

- Open thread from a message in chat, search, or user history to view retained
  replies and compose a response without replacing the main chat draft.
- Nested replies follow the original thread metadata. Missing root messages
  provide available quote context on hover.
- Live participation subscribes to new thread replies through the existing
  highlight and Mentions flow. Use the Subscribe to thread checkbox to change
  that choice. Historical messages and your own messages do not notify you.
- Thread drafts, pickers, and subscription choices belong to the active account.
  They remain available during the session; older replies outside retained
  channel history are not fetched automatically.
- Delayed send failures and upload results return to the originating composer
  when it is available; otherwise, they remain in unsent recovery. Rejected text
  is retained without replacing a newer draft. Targeted Twitch sends are rejected
  if the connected account has changed.

## Message views and shortcuts

- Global search and recent messages in user cards use the chat renderer for
  emotes, badges, replies, subscription messages, and context actions.
- Result body clicks navigate to the source message; sender names open user
  cards, and links keep their own actions. Cached rows retain keyboard and
  pointer behavior as the list scrolls.
- Settings > Hotkeys > Chat input exposes Send, Send and keep text, completion,
  history, Clear input, Cancel reply, and the emote picker. Default shortcuts
  include Ctrl+Enter to send while retaining text, Ctrl+U to clear, and Ctrl+E
  to open the picker.
- Shortcut conflict checks account for the focused surface and show the shifted
  Enter alternatives. Rebinding or clearing Send removes its default alternative.
- Text arriving after Send starts the next draft. Clear input and pane navigation
  preserve input order, including multiple keyboard events in one update.

## Emote loading

- Signed-in Twitch accounts fetch the complete native global catalog without
  requiring `user:read:emotes`. The bundled catalog remains the startup fallback.
- Global, personal, and follower catalog requests share a connection pool and
  request limit. Failed refreshes preserve usable data, and account changes reject
  stale results.
- Queued 7TV user lookups check whether they still belong to the active generation
  before starting HTTP work. Obsolete requests no longer consume the queue after
  a channel or account change.

The [pane and message-view audit](https://github.com/frankischilling/crust/blob/v0.6.0/docs/AUDIT_PANE_VIEWS.md)
records implementation details, measurements, and remaining work.
