# Crust v0.4.3 release notes

Date: 2026-04-06

v0.4.3 adds whisper threads and unread ordering to quick switch, plus message
moderation shortcuts. Workspace and internal crate versions are now 0.4.3.

## Highlights

- Quick switch now prioritizes mention items first, then unread items, then all others.
- Quick switch now includes whisper threads in the same searchable palette.
- Whisper threads now surface unread and mention badges in both thread lists and quick switch.
- Message row right-click menus now include one-click moderation shortcuts.

## Quick switch

The quick-switch palette (Ctrl+K) now combines channels and whisper threads in one list.

Behavior updates:

1. Mention-first ordering:
   - entries with mention badges appear first
   - entries with unread badges appear next
   - remaining entries are listed after
2. Unified search:
   - Twitch, Kick, and IRC channels are still supported
   - whisper threads are now included
   - whisper query aliases include:
     - whisper:<login>
     - w:<login>
     - @<display name>
3. Thread activation:
   - selecting a whisper thread opens/focuses the whisper panel
   - that thread is marked read when selected

## Whisper badges

Whisper thread lists now distinguish:

- unread whispers
- unread mentions of the active account

## Message moderation shortcuts

Right-clicking a message row now exposes immediate actions under Mod actions:

- Quick: Delete
- Quick: Timeout 10m
- Quick: Ban
- Quick: Warn

These are added alongside existing detailed moderation menus and workflows.

## Notes

- Existing advanced moderation menus (timeouts presets, low-trust actions, workflows, unban tooling) remain available.
- Quick-switch keyboard behavior is unchanged:
  - Ctrl+K opens
  - Up/Down navigates
  - Enter activates
  - Escape closes
