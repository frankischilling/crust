# Startup, private history, and navigation fixes

This pass starts from v0.5.2 and is tracked in
[issue #15](https://github.com/frankischilling/crust/issues/15), with connection
work in [issue #10](https://github.com/frankischilling/crust/issues/10) and room
snapshot work in [issue #11](https://github.com/frankischilling/crust/issues/11).

## Startup and image demand

Catalog loading previously spawned an image request for every emote and badge.
Those requests competed with images the user was trying to view. Saved history
and hidden conversations added more requests. The UI already had a bounded
cache that requests missing images during rendering, so these eager paths were
removed. Profile metadata also arrives without waiting for its avatar.

Provider catalogs publish independently. The channel-completion event still
waits for all providers to finish, while successful responses can already be
used in chat and the picker. A failed provider preserves its last usable set
and becomes retryable when the other providers finish. Catalog publication
reserves event capacity before capturing the snapshot, and channel generations
reject obsolete completions.

The startup screen is now a small status strip. It leaves chat, settings, and
navigation usable from the first frame. Cached catalog data does not count as a
successful network connection, and a disconnect clears readiness. The status
can be hidden while loading continues.

The live-feed widget renders visible rows. Other image-heavy widgets use the
shared UI cache, including messages, badges, autocomplete, the picker, profiles,
and shared-chat source images.

## Room snapshots and badge refresh

ROOMSTATE updates for joined channels reach the UI. History and pinned-message
snapshots load once per room and connection, with fresh loads after reconnect,
leave/rejoin, or a changed room ID. Badge requests have their own lifecycle, so a
retry does not repeat history work. Account changes and explicit emote refreshes
can refresh badges independently.

Each room connection owns its snapshot tasks and event sender. Leaving,
disconnecting, or replacing the room cancels the work. Before publishing a result,
the runtime checks that the request still belongs to the room, including after
waiting for event-queue capacity. A newer shared-session probe replaces the old
probe, including when the new result reports that the session ended.

Channel badge providers return an immutable snapshot. The runtime accepts it
only for the current request generation and replaces that channel's scope.
Successful empty responses remove obsolete definitions; failures preserve the
cache and allow a later retry. Cancelled and panicked jobs cannot leave a channel
permanently marked as loading.

Badge-cache saves run on a blocking worker. Refreshes share one pending save,
and the worker releases the map lock before serialization and disk writes.

## Whisper ownership

The old SQLite key contained only the conversation partner. Replay consequently
read private rows from every account and treated the selected account as their
recipient. New keys use `whisper:uid:<account_id>:<partner_login>`. Persistence and
replay require the authenticated account's immutable Twitch user ID. Persistence
also checks the sender or recipient against the current account, and replay reads
only that account ID's prefix. Prefix queries escape SQL wildcard characters.

Changing accounts aborts the previous replay task. An event-bridge guard rejects
private events for another account before plugin dispatch, and the UI performs
the same ownership check before updating threads or notifications. Reusing an
old login does not grant access to another account's history. The same account
keeps its history after a rename; replay uses its current login for local messages
and verifies stored self-sent messages against its user ID. Legacy rows with no
owner or only a login-based owner remain stored without being assigned to an
account. Authentication publishes the validated login, keeping localized display
names out of ownership and routing checks.

Delayed send failures carry their owner ID, so another account cannot receive
the previous account's recipient or error text. They appear as temporary notices
for the owning account. They never enter a shared channel buffer, and switching
accounts or logging out removes them.

Avatar responses also carry their account owner. Both the event bridge and the
UI reject an old account's response after a switch or logout.

Whispers retain the sender ID supplied by IRC or EventSub, including through
SQLite replay and plugin events. Personal emotes resolve against that sender,
and catalog refreshes retokenize each thread with its original sender IDs.
Observed senders share the existing bounded lookup queue. Missing IDs and old
login placeholders stay unidentified.

## EventSub recovery

The saved-account desktop run exposed repeated reconnects around the advertised
keepalive boundary. The client expired its watchdog at the same interval when
the server sends an idle keepalive, leaving no time for delivery or scheduling
delays. Established sessions now allow two extra seconds. A silent connection
still expires, and Ping frames or malformed messages do not refresh the deadline.
The initial connection and welcome deadlines remain ten seconds.

Valid notifications or keepalives received after thirty seconds of an established
session reset the reconnect delay. Earlier failures therefore cannot leave later
disconnects waiting at the maximum backoff. Account changes and shutdown cancel
the initial handshake and retry wait immediately.

## Live channel emotes

Joined Twitch channels receive BTTV and 7TV set changes over provider sockets.
The 7TV path also supports joined Kick channels. Additions, renames, and removals
update the channel catalog used by the picker, autocomplete, and message
refreshes without downloading images for hidden entries. Changing a channel's
active 7TV set replaces only that provider's definitions.

Each provider has one socket worker, with up to 64 joined channels and at most
four metadata requests in flight. The 7TV service's subscription limit can
reduce the number of watched channels. Desired subscriptions replace the
previous list. Room lifetimes, provider binding generations, and socket epochs
reject old work. A binding change starts a new socket connection so buffered
updates and acknowledgments from the old subscription cannot enter the
replacement set.

7TV reconnects subscribe first, wait for acknowledgments, then fetch a fresh
snapshot and replay changes received during that request. BTTV's public snapshot
endpoint is cached. Edits received while connected apply immediately. Observed
edits survive cached snapshots for 30 minutes; an offline change that contradicts
one of those observations may not appear until that window expires and the next
five-minute refresh runs. Later snapshots can then replace old observations,
including removals and re-adds that happened offline. Failed snapshots retry
after 30 seconds.

Global sets and personal 7TV sets keep their existing refresh paths. This change
does not add live subscriptions for those scopes or an FFZ event feed.

## Username paints

The provider's selected paint now reaches the renderer, including gradients,
image fills, and shadows. Current responses update recent Twitch messages and
mentions; other platforms with the same numeric user ID are unaffected. Clearing
a paint removes it. Failed requests and stale generations retain the last usable
style.

## Twitch emote inventory

The signed-in account's Twitch emotes now come from the paginated account
inventory. They feed the picker, autocomplete, and outgoing emote ranges. The
loader limits response size and work, rejects obsolete account generations, and
keeps the last complete inventory if a refresh fails.

This requires the optional `user:read:emotes` scope. Chat still works without it.
Follower emotes require an additional channel query and remain a follow-up.

## Chat interaction

Quick switch respects search focus, scrolls the selected item into view, and
changes pointer selection only when the pointer moves or clicks. Search fields
request focus once, use stable identities across layouts, and stop filtering
when closed. Duplicate panes keep their own scrolling, pause snapshots, and
layout state while channel changes still invalidate every visible copy.

Whisper Reply writes into the visible composer and asks the user to finish or
clear a nonempty draft. Join and login dialogs allow keyboard focus to leave
empty fields. The join dialog also fits narrow viewports and provides a close
control.

Shortcut capture runs before app shortcuts and consumes the captured keyboard
input. Binding Ctrl+K therefore cannot also open quick switch; Escape, Enter, and
Tab cannot act on another control in that frame. Leaving Hotkeys or closing
Settings cancels capture. Icon buttons show keyboard focus and supply descriptive
labels to accessibility tools.

Layout controls show the active layout. Tabs selects top tabs. Sidebar is disabled
when the window is too narrow, with a tooltip explaining why. The overflow menu
uses the same actions. Expired-token guidance points to the account button in the
top bar and Add Account.

Search header actions wrap in narrow windows, leaving the title and close control
clear. Message tokenization preserves Unicode whitespace and character boundaries
and ignores malformed emote ranges whose end precedes their start.

IRC private conversations use a separate internal identity from ordinary rooms.
Incoming messages address the sender's conversation; replies and actions use
the peer nickname. Query tabs never issue JOIN or PART. A successful private
send opens its UI conversation before publishing the echo, while a failed send
keeps the original draft. Service credential commands stay out of local echoes.

## Verification

Regression coverage exercises provider completion order, event backpressure,
badge generation and retry behavior, account-scoped SQLite replay, stale private
events, account renames and reused logins, initial connection cancellation, and
IRC private message routing. Inventory tests cover pagination and account changes;
tokenization tests cover Unicode text and malformed native-emote ranges.
Headless UI tests exercise the production widgets with keyboard, pointer, wheel,
and resize input, including chat input while startup is active.

The combined workspace gate passed 1,039 tests, with 11 ignored and no failures.
Correctness Clippy checks passed. Loopback WebSocket and HTTP tests cover provider
subscription acknowledgments, delayed snapshots, live removals, active-set
changes, and fresh subscriptions after reconnect. Reconciliation tests cover
offline edits, repeated events, overflow, and obsolete room or set lifetimes.
EventSub loopbacks cover delayed keepalives, idle expiry, replacement-session
timeouts, and retry recovery after healthy traffic.

Offline native captures confirmed that chat, settings, and search controls fit
at wide, narrow, and enlarged-text sizes. Saved-account desktop observations and
Windows and Linux release checks are recorded in
[PR #16](https://github.com/frankischilling/crust/pull/16).

The [v0.5.4 interface and performance audit](AUDIT_UI_PERFORMANCE.md) continues
this work with channel-specific follower inventory, global and personal live
updates, background animation decoding, and further chat interaction fixes.
