use std::collections::HashSet;

use crust_core::events::AppEvent;
use crust_core::model::{ChannelId, MAX_CHAT_PANES};
use crust_storage::AppSettings;

/// Keep channels that this startup can join, including those still connecting.
pub(crate) fn startup_restore_event(
    settings: &AppSettings,
    initial_channels: &[ChannelId],
    kick_enabled: bool,
    irc_enabled: bool,
) -> AppEvent {
    let eligible: HashSet<&ChannelId> = initial_channels
        .iter()
        .filter(|channel| {
            !channel.is_virtual()
                && (!channel.is_kick() || kick_enabled)
                && (!channel.is_irc() || irc_enabled)
        })
        .collect();
    let restore_channel = |raw: &str| {
        let sentinel = ChannelId(raw.trim().to_owned());
        if sentinel.is_virtual() {
            return Some(sentinel);
        }
        ChannelId::parse_user_input(raw).filter(|channel| eligible.contains(channel))
    };

    let channel = restore_channel(&settings.last_active_channel)
        .map(|channel| channel.0)
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let channel_order = settings
        .channel_order
        .iter()
        .filter_map(|raw| restore_channel(raw))
        .filter(|channel| !channel.is_virtual() && seen.insert(channel.clone()))
        .map(|channel| channel.0)
        .collect();

    // Keep original indices so removing an unavailable pane does not move focus
    // to a different surviving pane. Repeated channels can hold separate drafts.
    let panes: Vec<_> = settings
        .split_panes
        .iter()
        .enumerate()
        .filter_map(|(index, pane)| {
            restore_channel(&pane.channel).map(|channel| {
                let frac = if pane.frac.is_finite() && pane.frac > 0.0 {
                    pane.frac
                } else {
                    1.0
                };
                (index, channel.0, frac)
            })
        })
        .take(MAX_CHAT_PANES)
        .collect();
    let mut indices = vec![None; settings.split_panes.len()];
    for (new_index, (old_index, _, _)) in panes.iter().enumerate() {
        indices[*old_index] = Some(new_index);
    }
    let split_layout = settings.split_layout.as_ref()
        .and_then(|layout| layout.remap(&indices))
        .and_then(|layout| layout.validated(panes.len()));
    let split_views = if settings.split_panes.is_empty() {
        settings.split_pane_views.iter().take(1).cloned().collect()
    } else {
        panes.iter().map(|(old_index, _, _)| settings.split_pane_views.get(*old_index).cloned().unwrap_or_default()).collect()
    };
    let focused = panes
        .iter()
        .position(|(index, _, _)| *index >= settings.split_panes_focused)
        .unwrap_or_else(|| panes.len().saturating_sub(1));

    AppEvent::RestoreLastActiveChannel {
        channel,
        channel_order,
        split_panes: panes
            .into_iter()
            .map(|(_, channel, frac)| (channel, frac))
            .collect(),
        split_panes_focused: focused,
        split_layout,
        split_views,
        detached_chats: settings.detached_chats.iter().filter_map(|window| {
            let channel = restore_channel(&window.channel)?;
            if channel.is_virtual() { return None; }
            let mut window = window.clone().normalized();
            window.channel = channel.0;
            Some(window)
        }).take(crust_core::model::MAX_DETACHED_CHATS).collect(),
        whispers_visible: settings.whispers_panel_visible,
        last_whisper_login: settings.last_whisper_login.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crust_storage::settings::PersistedPane;

    fn pane(channel: &str, frac: f32) -> PersistedPane {
        PersistedPane {
            channel: channel.to_owned(),
            frac,
        }
    }

    #[test]
    fn startup_layout_canonicalizes_ids_and_drops_channels_that_cannot_join() {
        let settings = AppSettings {
            last_active_channel: "twitch:ALICE".into(),
            channel_order: [
                "kick:room",
                "#BOB",
                "twitch:Alice",
                "alice",
                "gone",
                "bad name",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            split_panes: vec![
                pane("kick:room", 0.2),
                pane("#Bob", 0.3),
                pane("twitch:ALICE", 0.5),
            ],
            split_panes_focused: 2,
            ..Default::default()
        };
        let initial = vec![
            ChannelId::new("alice"),
            ChannelId::new("bob"),
            ChannelId::kick("room"),
        ];
        let AppEvent::RestoreLastActiveChannel {
            channel,
            channel_order,
            split_panes,
            split_panes_focused,
            ..
        } = startup_restore_event(&settings, &initial, false, false)
        else {
            panic!()
        };
        assert_eq!(channel, "alice");
        assert_eq!(channel_order, ["bob", "alice"]);
        assert_eq!(split_panes, [("bob".into(), 0.3), ("alice".into(), 0.5)]);
        assert_eq!(split_panes_focused, 1);
    }

    #[test]
    fn startup_layout_prunes_nested_unavailable_panes_and_keeps_duplicate_views() {
        use crust_core::model::{PaneLayout, SplitAxis};
        let settings = AppSettings {
            split_panes: vec![pane("gone", 0.2), pane("room", 0.3), pane("room", 0.5)],
            split_panes_focused: 2,
            split_pane_views: ["gone query", "first query", "second query"].into_iter().map(|keyword| {
                crust_core::model::PaneViewSettings {
                    search: crust_core::model::PaneSearchSettings { open: true, keyword: keyword.into(), ..Default::default() },
                    filters: Some(vec![keyword.into()]),
                }
            }).collect(),
            split_layout: Some(PaneLayout::Split {
                axis: SplitAxis::Horizontal, fraction: 0.2,
                first: Box::new(PaneLayout::Pane { index: 0 }),
                second: Box::new(PaneLayout::Split {
                    axis: SplitAxis::Vertical, fraction: 0.35,
                    first: Box::new(PaneLayout::Pane { index: 1 }),
                    second: Box::new(PaneLayout::Pane { index: 2 }),
                }),
            }),
            ..Default::default()
        };
        let AppEvent::RestoreLastActiveChannel { split_layout, split_panes, split_panes_focused, split_views, .. } =
            startup_restore_event(&settings, &[ChannelId::new("room")], false, false) else { panic!() };
        assert_eq!(split_panes.len(), 2);
        assert_eq!(split_panes_focused, 1);
        assert_eq!(split_views.iter().map(|view| view.search.keyword.as_str()).collect::<Vec<_>>(), ["first query", "second query"]);
        assert_eq!(split_views[1].filters.as_ref().unwrap(), &["second query"]);
        assert_eq!(split_layout, Some(PaneLayout::Split {
            axis: SplitAxis::Vertical, fraction: 0.35,
            first: Box::new(PaneLayout::Pane { index: 0 }),
            second: Box::new(PaneLayout::Pane { index: 1 }),
        }));
    }

    #[test]
    fn startup_layout_keeps_enabled_delayed_joins_and_distinct_duplicate_panes() {
        let irc = ChannelId::parse_user_input("ircs://irc.example.test:6697/#room").unwrap();
        let initial = vec![ChannelId::kick("room"), irc.clone()];
        let settings = AppSettings {
            last_active_channel: "kick:ROOM".into(),
            channel_order: vec![
                "ircs://irc.example.test:6697/#room".into(),
                "kick:ROOM".into(),
            ],
            split_panes: vec![
                pane("gone", 0.1),
                pane("kick:ROOM", 0.3),
                pane("kick:room", 0.6),
            ],
            split_panes_focused: 2,
            ..Default::default()
        };
        let AppEvent::RestoreLastActiveChannel {
            channel,
            channel_order,
            split_panes,
            split_panes_focused,
            ..
        } = startup_restore_event(&settings, &initial, true, true)
        else {
            panic!()
        };
        assert_eq!(channel, "kick:room");
        assert_eq!(channel_order, [irc.0, "kick:room".into()]);
        assert_eq!(
            split_panes,
            [("kick:room".into(), 0.3), ("kick:room".into(), 0.6)]
        );
        assert_eq!(split_panes_focused, 1);

        let AppEvent::RestoreLastActiveChannel {
            channel,
            channel_order,
            split_panes,
            ..
        } = startup_restore_event(&settings, &initial, false, false)
        else {
            panic!()
        };
        assert!(channel.is_empty());
        assert!(channel_order.is_empty());
        assert!(split_panes.is_empty());
    }

    #[test]
    fn startup_layout_keeps_virtual_panes_and_bounds_invalid_saved_layouts() {
        let settings = AppSettings {
            last_active_channel: ChannelId::mentions().0,
            split_panes: vec![
                pane(ChannelId::live_feed().as_str(), f32::NAN),
                pane("removed", 0.2),
                pane(ChannelId::mentions().as_str(), f32::INFINITY),
                pane("alice", -1.0),
                pane("alice", 0.5),
                pane("alice", 0.5),
            ],
            split_panes_focused: usize::MAX,
            whispers_panel_visible: true,
            last_whisper_login: "friend".into(),
            ..Default::default()
        };
        let AppEvent::RestoreLastActiveChannel {
            channel,
            split_panes,
            split_panes_focused,
            whispers_visible,
            last_whisper_login,
            ..
        } = startup_restore_event(&settings, &[ChannelId::new("alice")], false, false)
        else {
            panic!()
        };
        assert_eq!(channel, ChannelId::mentions().0);
        assert_eq!(split_panes.len(), 5);
        assert_eq!(split_panes[0], (ChannelId::live_feed().0, 1.0));
        assert_eq!(split_panes[1], (ChannelId::mentions().0, 1.0));
        assert!(split_panes
            .iter()
            .all(|(_, frac)| frac.is_finite() && *frac > 0.0));
        assert_eq!(split_panes_focused, 4);
        assert!(whispers_visible);
        assert_eq!(last_whisper_login, "friend");
    }

    #[test]
    fn startup_detached_chats_keep_duplicate_views_and_prune_unavailable_channels() {
        use crust_core::model::DetachedChatSettings;
        let window = |channel: &str| DetachedChatSettings { channel: channel.into(), ..Default::default() };
        let settings = AppSettings { detached_chats: vec![window("gone"), window("twitch:ROOM"),
            window("room"), window("kick:room"), window(ChannelId::mentions().as_str())], ..Default::default() };
        let AppEvent::RestoreLastActiveChannel { detached_chats, .. } = startup_restore_event(&settings,
            &[ChannelId::new("room"), ChannelId::kick("room")], false, false) else { panic!() };
        assert_eq!(detached_chats.iter().map(|window| window.channel.as_str()).collect::<Vec<_>>(), ["room", "room"]);
    }
}
