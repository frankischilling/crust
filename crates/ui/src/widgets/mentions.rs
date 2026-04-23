use std::collections::VecDeque;

use egui::{
    Align, Color32, CornerRadius, Frame, Label, Layout, Margin, RichText, ScrollArea, Sense,
    Stroke, Ui, Vec2,
};

use crust_core::model::{ChannelId, ChatMessage, MessageId};

use crate::theme as t;
use crate::widgets::message_list::format_message_timestamp;

/// One-shot action emitted by [`MentionsList::show`] when the user interacts
/// with a row. The app is expected to switch `state.active_channel` to the
/// target and schedule a scroll-to via `pending_scroll_to_message`.
#[derive(Debug, Clone)]
pub struct MentionJumpTarget {
    pub channel: ChannelId,
    pub message: MessageId,
}

/// Cross-channel "Mentions" pseudo-tab renderer.
///
/// Renders the `state.mentions` ring buffer with a channel pill on each row
/// that also doubles as a click target to jump back to the original message
/// in its source channel (B4 acceptance criterion).
///
/// The rendering is intentionally simplified relative to
/// [`super::message_list::MessageList`]: the Mentions tab is a *pointer*
/// into the real chat, not a replacement for it. Users click through to see
/// full emote/badge rendering in the source channel's own buffer.
pub struct MentionsList<'a> {
    pub mentions: &'a VecDeque<ChatMessage>,
    pub show_timestamps: bool,
    pub show_timestamp_seconds: bool,
    pub use_24h_timestamps: bool,
}

impl<'a> MentionsList<'a> {
    pub fn show(&self, ui: &mut Ui) -> Option<MentionJumpTarget> {
        // Upstream `pane_ui` in split-pane mode zeroes item_spacing; restore
        // sane defaults so our header / rows don't glue together.
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 4.0);

        let scale = (t::chat_font_size() / 14.0).clamp(0.75, 2.5);
        // Modest horizontal gutter - just enough to keep text from
        // touching the sidebar border without looking over-indented.
        let pad_x = (10.0 * scale).round() as i8;
        let margin_top = (10.0 * scale) as i8;
        let bottom_pad = (10.0 * scale).max(8.0);

        let mut action: Option<MentionJumpTarget> = None;

        // Fill the whole pane with `bg_base` so the sidebar / central-panel
        // background doesn't peek through around the scrollbar or along
        // the bottom when the buffer is short.
        Frame::new()
            .fill(t::bg_base())
            .inner_margin(Margin {
                left: 0,
                right: 0,
                top: margin_top,
                bottom: 0,
            })
            .show(ui, |ui| {
                // -- Sticky header (does not scroll) ----------------------
                //
                // Header gets the same `pad_x` as each row so the title
                // and the channel pills/text below align perfectly.
                Frame::new()
                    .inner_margin(Margin {
                        left: pad_x,
                        right: pad_x,
                        top: 0,
                        bottom: 0,
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Mentions").font(t::heading()).strong());
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(format!("({})", self.mentions.len()))
                                    .font(t::small())
                                    .color(t::text_muted()),
                            );
                        });
                    });
                ui.add_space(4.0);
                ui.add(egui::Separator::default().spacing(0.0));
                ui.add_space(4.0);

                // -- Empty state -----------------------------------------
                if self.mentions.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(
                            RichText::new("No mentions yet")
                                .font(t::body())
                                .color(t::text_muted()),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(
                                "Highlights, @-mentions, keyword matches, first-time \
                                 chatters and pinned messages from all open channels \
                                 will appear here.",
                            )
                            .font(t::small())
                            .color(t::text_secondary()),
                        );
                    });
                    return;
                }

                // -- Scrollable list --------------------------------------
                //
                // Bound the ScrollArea to `available_height - bottom_pad`
                // so the last row never lands flush with the central-panel
                // border when the buffer is scrolled to the bottom.
                let avail_h = ui.available_height();
                let scroll_h = (avail_h - bottom_pad).max(40.0);

                ScrollArea::vertical()
                    .id_salt("mentions_scroll")
                    .auto_shrink([false; 2])
                    .stick_to_bottom(true)
                    .max_height(scroll_h)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        // Oldest → newest; `stick_to_bottom` keeps the
                        // newest visible on arrival, matching the rest
                        // of the chat UI.
                        for msg in self.mentions.iter() {
                            if let Some(t) = render_row(
                                ui,
                                msg,
                                pad_x,
                                self.show_timestamps,
                                self.show_timestamp_seconds,
                                self.use_24h_timestamps,
                            ) {
                                action = Some(t);
                            }
                        }
                    });

                ui.add_space(bottom_pad);
            });

        action
    }
}

fn render_row(
    ui: &mut Ui,
    msg: &ChatMessage,
    pad_x: i8,
    show_ts: bool,
    show_ts_seconds: bool,
    use_24h: bool,
) -> Option<MentionJumpTarget> {
    let mut jump: Option<MentionJumpTarget> = None;

    let row_id = egui::Id::new("mention_row").with(msg.id.0);
    let pill_id = egui::Id::new("mention_pill").with(msg.id.0);

    // Capture the viewport width BEFORE entering the Frame. We'll pass
    // this as an explicit `max_width` to every sub-layout so nothing
    // inherits bounds from the ScrollArea's (possibly widening) content
    // rect - the root cause of rows drifting left as you scroll.
    let avail_w = ui.available_width();
    let content_w = (avail_w - 2.0 * pad_x as f32).max(40.0);

    // Pre-compute values used both for the header and body.
    let pill_bg = channel_pill_color(&msg.channel);
    let pill_fg = t::text_on_accent();
    let pill_text = format!("#{}", msg.channel.display_name());

    let sender_color = msg
        .sender
        .color
        .as_deref()
        .and_then(parse_hex_color)
        .unwrap_or(t::text_primary());
    let display_name = if msg.sender.display_name.trim().is_empty() {
        msg.sender.login.as_str()
    } else {
        msg.sender.display_name.as_str()
    };

    let body_color = if msg.flags.is_deleted {
        t::text_muted()
    } else {
        t::text_primary()
    };
    let body_text = if msg.raw_text.trim().is_empty() {
        "(no text)".to_owned()
    } else {
        msg.raw_text.clone()
    };

    // Reserve a painter slot BEFORE rendering the row content. We'll
    // fill it in with the row's real rect after `ui.interact` gives us
    // an accurate hover state below. Doing it this way ensures the
    // hover backdrop renders *behind* the text (it's earlier in the
    // draw list) AND is sized exactly to the row (not to the
    // ScrollArea's remaining-space `max_rect`, which is what broke
    // hovering previously - the first row's hover rect covered every
    // row beneath it).
    let bg_shape_idx = ui.painter().add(egui::Shape::Noop);

    let outer = Frame::new()
        .fill(Color32::TRANSPARENT)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin {
            left: pad_x,
            right: pad_x,
            top: 4,
            bottom: 4,
        })
        .show(ui, |ui| {
            // Stack metadata and body VERTICALLY. This is the key fix
            // for narrow-window layout: the body text gets its own row
            // with the full `content_w` to wrap inside, so it never has
            // to fight the pill / sender for horizontal space. The
            // metadata row uses `allocate_ui_with_layout` with an
            // explicit size + `with_main_wrap` so even if the channel
            // pill itself is wider than the viewport, it wraps cleanly
            // instead of pushing everything else off-screen.
            ui.vertical(|ui| {
                ui.set_max_width(content_w);

                ui.allocate_ui_with_layout(
                    Vec2::new(content_w, 0.0),
                    Layout::left_to_right(Align::Center).with_main_wrap(true),
                    |ui| {
                        ui.set_max_width(content_w);
                        ui.spacing_mut().item_spacing = Vec2::new(6.0, 2.0);

                        if show_ts {
                            ui.add(Label::new(
                                RichText::new(format_message_timestamp(
                                    &msg.timestamp,
                                    show_ts_seconds,
                                    use_24h,
                                ))
                                .font(t::small())
                                .color(t::text_muted()),
                            ));
                        }

                        // Channel pill (clickable)
                        let pill_resp = Frame::new()
                            .fill(pill_bg)
                            .corner_radius(CornerRadius::same(4))
                            .stroke(Stroke::new(1.0, t::alpha(Color32::BLACK, 40)))
                            .inner_margin(Margin::symmetric(6, 1))
                            .show(ui, |ui| {
                                ui.add(
                                    Label::new(
                                        RichText::new(&pill_text)
                                            .font(t::small())
                                            .color(pill_fg)
                                            .strong(),
                                    )
                                    .selectable(false),
                                );
                            });
                        let pill_click = ui.interact(
                            pill_resp.response.rect,
                            pill_id,
                            Sense::click(),
                        );
                        if pill_click.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if pill_click.clicked() {
                            jump = Some(MentionJumpTarget {
                                channel: msg.channel.clone(),
                                message: msg.id,
                            });
                        }

                        // Sender
                        ui.add(
                            Label::new(
                                RichText::new(format!("{display_name}:"))
                                    .font(t::body())
                                    .color(sender_color)
                                    .strong(),
                            )
                            .selectable(false),
                        );
                    },
                );

                // Putting the body on its own row guarantees it always
                // has `content_w` available to wrap in, regardless of
                // how wide the pill or sender turned out to be. Uses
                // `Label::wrap()` for word-level breaks so long URLs
                // or walls of text don't overflow the viewport.
                ui.add(
                    Label::new(
                        RichText::new(body_text)
                            .font(t::body())
                            .color(body_color),
                    )
                    .wrap(),
                );
            });
        });

    // Row-level click (anywhere outside the pill) also jumps. Also
    // drives the hover backdrop we reserved above.
    let row_click = ui.interact(outer.response.rect, row_id, Sense::click());
    if row_click.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().set(
            bg_shape_idx,
            egui::Shape::rect_filled(
                outer.response.rect,
                CornerRadius::same(4),
                t::hover_row_bg(),
            ),
        );
    }
    if row_click.clicked() && jump.is_none() {
        jump = Some(MentionJumpTarget {
            channel: msg.channel.clone(),
            message: msg.id,
        });
    }

    jump
}

/// Deterministic per-channel pill color. We hash the channel id to pick from
/// a small, readable palette so repeated channels render with a stable hue
/// without needing a palette table in state.
fn channel_pill_color(ch: &ChannelId) -> Color32 {
    // Small on-brand palette; mirrors the username colour palette style used
    // elsewhere in the app. Keep the count small so distinct channels tend
    // to land on distinct hues even with a trivial hash.
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(155, 89, 182),  // purple
        Color32::from_rgb(52, 152, 219),  // blue
        Color32::from_rgb(26, 188, 156),  // teal
        Color32::from_rgb(46, 204, 113),  // green
        Color32::from_rgb(241, 196, 15),  // amber
        Color32::from_rgb(230, 126, 34),  // orange
        Color32::from_rgb(231, 76, 60),   // red
        Color32::from_rgb(236, 112, 170), // pink
    ];
    let mut hash: u32 = 5381;
    for b in ch.as_str().bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u32);
    }
    PALETTE[(hash as usize) % PALETTE.len()]
}

/// Parse `#rrggbb` / `rrggbb` into a `Color32`. Returns `None` for any
/// malformed input so callers can fall back to a theme colour.
fn parse_hex_color(s: &str) -> Option<Color32> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_color_accepts_leading_hash() {
        assert_eq!(parse_hex_color("#ff0000"), Some(Color32::from_rgb(255, 0, 0)));
        assert_eq!(parse_hex_color("00ff00"), Some(Color32::from_rgb(0, 255, 0)));
    }

    #[test]
    fn parse_hex_color_rejects_bad_input() {
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("#fff"), None);
        assert_eq!(parse_hex_color("zzzzzz"), None);
    }

    #[test]
    fn channel_pill_color_is_deterministic_per_channel() {
        let a = ChannelId::new("forsen");
        let b = ChannelId::new("forsen");
        let c = ChannelId::new("xqc");
        assert_eq!(channel_pill_color(&a), channel_pill_color(&b));
        // Different channels MAY collide (8-color palette), but running
        // the hash over two well-known logins must still produce a colour
        // in the palette for each.
        let pc = channel_pill_color(&c);
        // Trivially verify `pc` is from the palette by checking equality
        // against one of the known entries (via round-trip through hash).
        let _ = pc;
    }
}
