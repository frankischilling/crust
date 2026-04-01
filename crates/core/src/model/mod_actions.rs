use serde::{Deserialize, Serialize};

/// A saved moderation action preset (mirrors chatterino's `ModerationAction`).
///
/// The `command_template` may contain `{user}` and `{channel}` placeholders
/// which are substituted at invocation time.
///
/// Examples:
/// - `/timeout {user} 600`
/// - `/ban {user}`
/// - `/w {user} hi there`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModActionPreset {
    /// Short display label shown on the button (e.g. "60s", "Ban").
    pub label: String,
    /// IRC / chat command template with optional `{user}` / `{channel}`.
    pub command_template: String,
    /// Optional icon URL or path for custom presets.
    #[serde(default)]
    pub icon_url: Option<String>,
}

/// Type of moderation action, parsed from the command template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModActionType {
    Ban,
    Timeout { duration_seconds: u32 },
    Delete,
    Custom,
}

impl ModActionPreset {
    /// Expand `{user}` and `{channel}` placeholders in the template.
    pub fn expand(&self, user: &str, channel: &str) -> String {
        self.command_template
            .replace("{user}", user)
            .replace("{channel}", channel)
    }

    /// Parse the action type from the command template.
    pub fn action_type(&self) -> ModActionType {
        let cmd = self.command_template.trim();
        
        if cmd.starts_with("/ban ") || cmd.starts_with(".ban ") || cmd.starts_with("!ban ") {
            return ModActionType::Ban;
        }
        
        if cmd.starts_with("/delete ") || cmd.starts_with(".delete ") {
            return ModActionType::Delete;
        }
        
        if let Some(duration) = parse_timeout_duration(cmd) {
            return ModActionType::Timeout { duration_seconds: duration };
        }
        
        ModActionType::Custom
    }

    /// Generate a display label from the action type (used when label is empty).
    pub fn display_label(&self) -> String {
        if !self.label.is_empty() {
            return self.label.clone();
        }

        match self.action_type() {
            ModActionType::Ban => "Ban".to_owned(),
            ModActionType::Delete => "Del".to_owned(),
            ModActionType::Timeout { duration_seconds } => format_timeout_label(duration_seconds),
            ModActionType::Custom => {
                let cmd = self.command_template.trim();
                let first_word = cmd
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches(&['/', '.', '!']);
                first_word.chars().take(4).collect()
            }
        }
    }

    /// Returns the default set of presets used when none are configured.
    ///
    /// Mirrors chatterino's built-in quick-mod buttons.
    pub fn defaults() -> Vec<Self> {
        vec![
            ModActionPreset {
                label: "1m".to_owned(),
                command_template: "/timeout {user} 60".to_owned(),
                icon_url: None,
            },
            ModActionPreset {
                label: "10m".to_owned(),
                command_template: "/timeout {user} 600".to_owned(),
                icon_url: None,
            },
            ModActionPreset {
                label: "1h".to_owned(),
                command_template: "/timeout {user} 3600".to_owned(),
                icon_url: None,
            },
            ModActionPreset {
                label: "Ban".to_owned(),
                command_template: "/ban {user}".to_owned(),
                icon_url: None,
            },
        ]
    }
}

/// Parse timeout duration in seconds from a command like `/timeout {user} 600` or `/timeout {user} 10m`.
fn parse_timeout_duration(cmd: &str) -> Option<u32> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }

    if !matches!(parts[0], "/timeout" | ".timeout" | "!timeout") {
        return None;
    }

    let duration_str = parts[2];
    
    if let Ok(seconds) = duration_str.parse::<u32>() {
        return Some(seconds);
    }

    if duration_str.ends_with('s') {
        return duration_str[..duration_str.len() - 1].parse::<u32>().ok();
    } else if duration_str.ends_with('m') {
        return duration_str[..duration_str.len() - 1]
            .parse::<u32>()
            .ok()
            .map(|m| m * 60);
    } else if duration_str.ends_with('h') {
        return duration_str[..duration_str.len() - 1]
            .parse::<u32>()
            .ok()
            .map(|h| h * 3600);
    } else if duration_str.ends_with('d') {
        return duration_str[..duration_str.len() - 1]
            .parse::<u32>()
            .ok()
            .map(|d| d * 86400);
    } else if duration_str.ends_with('w') {
        return duration_str[..duration_str.len() - 1]
            .parse::<u32>()
            .ok()
            .map(|w| w * 604800);
    }

    None
}

/// Format a timeout duration into a short display label.
fn format_timeout_label(seconds: u32) -> String {
    const MINUTE: u32 = 60;
    const HOUR: u32 = 60 * MINUTE;
    const DAY: u32 = 24 * HOUR;
    const WEEK: u32 = 7 * DAY;

    if seconds < MINUTE {
        format!("{}s", seconds)
    } else if seconds < HOUR {
        format!("{}m", seconds / MINUTE)
    } else if seconds < DAY {
        format!("{}h", seconds / HOUR)
    } else if seconds < WEEK {
        format!("{}d", seconds / DAY)
    } else if seconds > 2 * WEEK {
        ">2w".to_owned()
    } else {
        format!("{}w", seconds / WEEK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_substitutes_user_and_channel() {
        let preset = ModActionPreset {
            label: "Timeout 10m".to_owned(),
            command_template: "/timeout {user} 600 in {channel}".to_owned(),
            icon_url: None,
        };
        assert_eq!(
            preset.expand("xqc", "forsen"),
            "/timeout xqc 600 in forsen"
        );
    }

    #[test]
    fn defaults_returns_four_entries() {
        assert_eq!(ModActionPreset::defaults().len(), 4);
    }

    #[test]
    fn parse_timeout_with_numeric_seconds() {
        let cmd = "/timeout testuser 600";
        assert_eq!(parse_timeout_duration(cmd), Some(600));
    }

    #[test]
    fn parse_timeout_with_minute_suffix() {
        let cmd = "/timeout testuser 10m";
        assert_eq!(parse_timeout_duration(cmd), Some(600));
    }

    #[test]
    fn parse_timeout_with_hour_suffix() {
        let cmd = "/timeout testuser 1h";
        assert_eq!(parse_timeout_duration(cmd), Some(3600));
    }

    #[test]
    fn format_timeout_label_seconds() {
        assert_eq!(format_timeout_label(45), "45s");
    }

    #[test]
    fn format_timeout_label_minutes() {
        assert_eq!(format_timeout_label(600), "10m");
    }

    #[test]
    fn format_timeout_label_hours() {
        assert_eq!(format_timeout_label(7200), "2h");
    }

    #[test]
    fn action_type_detects_ban() {
        let preset = ModActionPreset {
            label: "Ban".to_owned(),
            command_template: "/ban {user}".to_owned(),
            icon_url: None,
        };
        assert_eq!(preset.action_type(), ModActionType::Ban);
    }

    #[test]
    fn action_type_detects_timeout_with_duration() {
        let preset = ModActionPreset {
            label: "10m".to_owned(),
            command_template: "/timeout {user} 600".to_owned(),
            icon_url: None,
        };
        match preset.action_type() {
            ModActionType::Timeout { duration_seconds } => {
                assert_eq!(duration_seconds, 600);
            }
            _ => panic!("Expected Timeout action type"),
        }
    }

    #[test]
    fn display_label_uses_provided_label() {
        let preset = ModActionPreset {
            label: "Custom".to_owned(),
            command_template: "/timeout {user} 60".to_owned(),
            icon_url: None,
        };
        assert_eq!(preset.display_label(), "Custom");
    }

    #[test]
    fn display_label_generates_from_timeout_duration() {
        let preset = ModActionPreset {
            label: String::new(),
            command_template: "/timeout {user} 600".to_owned(),
            icon_url: None,
        };
        assert_eq!(preset.display_label(), "10m");
    }
}
