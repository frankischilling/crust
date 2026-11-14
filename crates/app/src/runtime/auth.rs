use std::time::Duration;

use crust_core::{AppCommand, AppEvent};
use crust_storage::AppSettings;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValidationKind {
    Startup,
    Login,
    AddAccount,
    SwitchAccount,
    Refresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidationRequest {
    pub id: u64,
    pub kind: ValidationKind,
    expected_login: Option<String>,
}

pub(crate) struct TokenValidationResult {
    pub request: ValidationRequest,
    pub token: String,
    pub result: Result<ValidateInfo, ValidateError>,
}

/// Only the latest requested account transition may update credentials.
#[derive(Default)]
pub(crate) struct AuthValidationRequests {
    next_id: u64,
    pending: Option<ValidationRequest>,
}

impl AuthValidationRequests {
    pub fn begin(
        &mut self,
        kind: ValidationKind,
        expected_login: Option<String>,
    ) -> ValidationRequest {
        self.next_id = self.next_id.wrapping_add(1);
        let request = ValidationRequest {
            id: self.next_id,
            kind,
            expected_login,
        };
        self.pending = Some(request.clone());
        request
    }

    pub fn cancel(&mut self) {
        self.pending = None;
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Identity-sensitive actions require an established account. A pending
    /// refresh may keep using that account because it cannot change its owner.
    pub fn action_rejection(
        &self,
        command: &AppCommand,
        authenticated: bool,
        connecting: bool,
    ) -> Option<AppEvent> {
        let action = twitch_account_action(command)?;
        let changing_account = self
            .pending
            .as_ref()
            .is_some_and(|request| request.kind != ValidationKind::Refresh);
        if authenticated && !changing_account {
            return None;
        }

        let reason = if self.is_pending() || connecting {
            "Sign-in is still in progress. Wait until your account is connected, then try again."
        } else {
            "You are signed out. Select an account, then try again."
        };
        match command {
            AppCommand::SendMessage {
                channel,
                text,
                reply_to_msg_id,
                reply,
            } => Some(AppEvent::MessageSendRejected {
                channel: channel.clone(),
                text: text.clone(),
                reply_to_msg_id: reply_to_msg_id.clone(),
                reply: reply.clone(),
                reason: format!("Message was not sent. {reason}"),
            }),
            AppCommand::SendWhisper { target_login, .. } => Some(AppEvent::Error {
                context: "SendWhisper".into(),
                message: format!("Whisper to @{target_login} was not sent. {reason}"),
            }),
            _ => Some(AppEvent::Error {
                context: "Account".into(),
                message: format!("{action} was not run. {reason}"),
            }),
        }
    }

    pub fn finish(
        &mut self,
        mut completion: TokenValidationResult,
    ) -> Option<TokenValidationResult> {
        if self.pending.as_ref() != Some(&completion.request) {
            return None;
        }
        self.pending = None;
        if let (Some(expected), Ok(info)) = (
            completion.request.expected_login.as_deref(),
            completion.result.as_ref(),
        ) {
            if !expected.eq_ignore_ascii_case(&info.login) {
                completion.result = Err(ValidateError::WrongAccount {
                    expected: expected.to_owned(),
                    actual: info.login.clone(),
                });
            }
        }
        Some(completion)
    }
}

/// Explicit account changes cannot fall back to the previous identity if
/// validation fails while later commands are still waiting in the UI queue.
pub(crate) fn detach_active_account(
    settings: &mut AppSettings,
    username: &mut Option<String>,
    user_id: &mut Option<String>,
    scopes: &mut Vec<String>,
    client_id: &mut Option<String>,
    expected_irc_identity: &mut Option<(u64, String)>,
) {
    settings.username.clear();
    settings.oauth_token.clear();
    *username = None;
    *user_id = None;
    scopes.clear();
    *client_id = None;
    *expected_irc_identity = None;
}

fn twitch_account_action(command: &AppCommand) -> Option<&'static str> {
    match command {
        AppCommand::SendMessage { channel, .. } if channel.is_twitch() => Some("Message"),
        AppCommand::SendWhisper { .. } => Some("Whisper"),
        AppCommand::TimeoutUser { .. }
        | AppCommand::BanUser { .. }
        | AppCommand::UnbanUser { .. }
        | AppCommand::WarnUser { .. }
        | AppCommand::DeleteMessage { .. }
        | AppCommand::SetSuspiciousUser { .. }
        | AppCommand::ClearSuspiciousUser { .. }
        | AppCommand::ResolveAutoModMessage { .. }
        | AppCommand::FetchUnbanRequests { .. }
        | AppCommand::ResolveUnbanRequest { .. }
        | AppCommand::SetShieldMode { .. } => Some("Moderation action"),
        AppCommand::UpdateRewardRedemptionStatus { .. }
        | AppCommand::CreatePoll { .. }
        | AppCommand::EndPoll { .. }
        | AppCommand::CreatePrediction { .. }
        | AppCommand::LockPrediction { .. }
        | AppCommand::ResolvePrediction { .. }
        | AppCommand::CancelPrediction { .. }
        | AppCommand::StartCommercial { .. }
        | AppCommand::CreateStreamMarker { .. }
        | AppCommand::SendAnnouncement { .. }
        | AppCommand::SendShoutout { .. }
        | AppCommand::UpdateChannelInfo { .. } => Some("Channel action"),
        _ => None,
    }
}

pub(crate) fn spawn_validation(
    tx: mpsc::Sender<TokenValidationResult>,
    request: ValidationRequest,
    token: String,
) {
    tokio::spawn(async move {
        let result = validate_token(&token).await;
        let _ = tx
            .send(TokenValidationResult {
                request,
                token,
                result,
            })
            .await;
    });
}

pub(crate) fn save_validated_account(settings: &mut AppSettings, token: &str, info: &ValidateInfo) {
    settings.username = info.login.clone();
    settings.oauth_token = token.to_owned();
    if let Some(account) = settings
        .accounts
        .iter_mut()
        .find(|account| account.username.eq_ignore_ascii_case(&info.login))
    {
        account.username = info.login.clone();
        account.oauth_token = token.to_owned();
    } else {
        settings.accounts.push(crust_storage::AccountEntry {
            username: info.login.clone(),
            oauth_token: token.to_owned(),
        });
    }
}

#[derive(Debug)]
pub(crate) enum ValidateError {
    Unauthorized,
    WrongAccount { expected: String, actual: String },
    Transient(String),
}

impl std::fmt::Display for ValidateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "token rejected by Twitch (unauthorized)"),
            Self::WrongAccount { expected, actual } => {
                write!(
                    f,
                    "saved token belongs to {actual}, not {expected}; add the account again"
                )
            }
            Self::Transient(error) => write!(f, "{error}"),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ValidateInfo {
    pub login: String,
    pub user_id: String,
    pub client_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

pub(crate) async fn validate_token(token: &str) -> Result<ValidateInfo, ValidateError> {
    validate_token_at(
        token,
        "https://id.twitch.tv/oauth2/validate",
        Duration::from_secs(10),
    )
    .await
}

async fn validate_token_at(
    token: &str,
    url: &str,
    timeout: Duration,
) -> Result<ValidateInfo, ValidateError> {
    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    let response = reqwest::Client::new()
        .get(url)
        .timeout(timeout)
        .header("Authorization", format!("OAuth {bare}"))
        .send()
        .await
        .map_err(|error| ValidateError::Transient(format!("HTTP error: {error}")))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(ValidateError::Unauthorized);
    }
    if !status.is_success() {
        return Err(ValidateError::Transient(format!(
            "Token validation failed (HTTP {status})"
        )));
    }
    let mut info = response.json::<ValidateInfo>().await.map_err(|error| {
        ValidateError::Transient(format!("Invalid validation response: {error}"))
    })?;
    if info.login.is_empty() || info.user_id.is_empty() || info.client_id.is_empty() {
        return Err(ValidateError::Transient(
            "Token validation response is missing account identity".into(),
        ));
    }
    info.login.make_ascii_lowercase();
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn validated(request: ValidationRequest, login: &str) -> TokenValidationResult {
        TokenValidationResult {
            request,
            token: format!("{login}-token"),
            result: Ok(ValidateInfo {
                login: login.into(),
                user_id: format!("{login}-id"),
                client_id: format!("{login}-client"),
                scopes: vec![format!("{login}-scope")],
            }),
        }
    }

    fn chat_command() -> AppCommand {
        AppCommand::SendMessage {
            channel: crust_core::ChannelId::new("room"),
            text: "message for the selected account".into(),
            reply_to_msg_id: None,
            reply: None,
        }
    }

    #[test]
    fn account_changes_block_actions_until_the_new_identity_connects() {
        for kind in [
            ValidationKind::Login,
            ValidationKind::AddAccount,
            ValidationKind::SwitchAccount,
        ] {
            let mut requests = AuthValidationRequests::default();
            let mut settings = AppSettings::default();
            let old = validated(requests.begin(ValidationKind::Login, None), "alice");
            save_validated_account(&mut settings, &old.token, old.result.as_ref().unwrap());
            requests.finish(old).unwrap();
            let mut username = Some("alice".into());
            let mut user_id = Some("alice-id".into());
            let mut scopes = vec!["alice-scope".into()];
            let mut client_id = Some("alice-client".into());
            let mut expected = Some((1, "alice-id".into()));

            detach_active_account(
                &mut settings,
                &mut username,
                &mut user_id,
                &mut scopes,
                &mut client_id,
                &mut expected,
            );
            let request = requests.begin(kind, Some("bob".into()));
            assert!(settings.username.is_empty());
            assert!(settings.oauth_token.is_empty());
            assert!(username.is_none() && user_id.is_none() && client_id.is_none());
            assert!(scopes.is_empty() && expected.is_none());
            assert_eq!(settings.accounts[0].oauth_token, "alice-token");
            let commands = [
                chat_command(),
                AppCommand::SendWhisper {
                    target_login: "viewer".into(),
                    text: "private message".into(),
                },
                AppCommand::BanUser {
                    channel: crust_core::ChannelId::new("room"),
                    login: "viewer".into(),
                    user_id: "viewer-id".into(),
                    reason: None,
                },
            ];
            for command in &commands {
                assert!(requests
                    .action_rejection(command, user_id.is_some(), expected.is_some())
                    .is_some());
            }

            let completion = requests.finish(validated(request, "bob")).unwrap();
            let info = completion.result.unwrap();
            save_validated_account(&mut settings, &completion.token, &info);
            expected = Some((completion.request.id, info.user_id.clone()));
            for command in &commands {
                assert!(
                    requests
                        .action_rejection(command, user_id.is_some(), expected.is_some())
                        .is_some(),
                    "token validation alone must not release chat actions"
                );
            }
            username = Some(info.login);
            user_id = Some(info.user_id);
            assert_eq!(username.as_deref(), Some("bob"));
            for command in &commands {
                assert!(requests
                    .action_rejection(command, user_id.is_some(), expected.is_some())
                    .is_none());
            }
        }
    }

    #[test]
    fn delayed_actions_after_failed_or_cancelled_validation_cannot_use_the_old_account() {
        let mut requests = AuthValidationRequests::default();
        let mut settings = AppSettings::default();
        settings.username = "alice".into();
        settings.oauth_token = "alice-token".into();
        let mut username = Some("alice".into());
        let mut user_id = Some("alice-id".into());
        let mut scopes = vec!["chat:edit".into()];
        let mut client_id = Some("alice-client".into());
        let mut expected = Some((1, "alice-id".into()));
        detach_active_account(
            &mut settings,
            &mut username,
            &mut user_id,
            &mut scopes,
            &mut client_id,
            &mut expected,
        );
        let failed = requests.begin(ValidationKind::SwitchAccount, Some("bob".into()));
        requests
            .finish(TokenValidationResult {
                request: failed.clone(),
                token: "invalid-token".into(),
                result: Err(ValidateError::Unauthorized),
            })
            .unwrap();
        assert!(!requests.is_pending());
        assert!(
            matches!(requests.action_rejection(&chat_command(), user_id.is_some(), expected.is_some()), Some(AppEvent::MessageSendRejected { reason, .. }) if reason.contains("signed out"))
        );

        let newer = requests.begin(ValidationKind::SwitchAccount, Some("carol".into()));
        assert!(requests.finish(validated(failed, "bob")).is_none());
        assert!(requests.is_pending());
        assert!(requests
            .action_rejection(&chat_command(), user_id.is_some(), expected.is_some())
            .is_some());
        requests.cancel();
        assert!(requests.finish(validated(newer, "carol")).is_none());
        assert!(!requests.is_pending());
        assert!(requests
            .action_rejection(&chat_command(), user_id.is_some(), expected.is_some())
            .is_some());
        assert!(settings.oauth_token.is_empty());
    }

    #[test]
    fn rejected_chat_keeps_the_body_and_full_reply_for_recovery() {
        let requests = AuthValidationRequests::default();
        let reply = crust_core::ReplyInfo {
            parent_msg_id: "parent-id".into(),
            parent_user_login: "viewer".into(),
            parent_display_name: "Viewer".into(),
            parent_msg_body: "quoted message".into(),
        };
        let command = AppCommand::SendMessage {
            channel: crust_core::ChannelId::new("room"),
            text: "  reply with spaces 🦀  ".into(),
            reply_to_msg_id: Some("parent-id".into()),
            reply: Some(reply),
        };
        match requests.action_rejection(&command, false, true).unwrap() {
            AppEvent::MessageSendRejected {
                channel,
                text,
                reply_to_msg_id,
                reply,
                reason,
            } => {
                assert_eq!(channel, crust_core::ChannelId::new("room"));
                assert_eq!(text, "  reply with spaces 🦀  ");
                assert_eq!(reply_to_msg_id.as_deref(), Some("parent-id"));
                assert_eq!(reply.unwrap().parent_msg_body, "quoted message");
                assert!(reason.contains("not sent"));
            }
            event => panic!("expected a recoverable rejected message, got {event:?}"),
        }
    }

    #[test]
    fn account_barrier_covers_moderation_and_broadcaster_actions() {
        use serde_json::json;

        let commands = [
            json!({"TimeoutUser":{"channel":"room","login":"viewer","user_id":"2","seconds":60,"reason":null}}),
            json!({"BanUser":{"channel":"room","login":"viewer","user_id":"2","reason":null}}),
            json!({"UnbanUser":{"channel":"room","login":"viewer","user_id":"2"}}),
            json!({"WarnUser":{"channel":"room","login":"viewer","user_id":"2","reason":"warning"}}),
            json!({"DeleteMessage":{"channel":"room","message_id":"message"}}),
            json!({"SetSuspiciousUser":{"channel":"room","login":"viewer","user_id":"2","restricted":true}}),
            json!({"ClearSuspiciousUser":{"channel":"room","login":"viewer","user_id":"2"}}),
            json!({"ResolveAutoModMessage":{"channel":"room","message_id":"message","sender_user_id":"2","action":"ALLOW"}}),
            json!({"FetchUnbanRequests":{"channel":"room"}}),
            json!({"ResolveUnbanRequest":{"channel":"room","request_id":"request","approve":true,"resolution_text":null}}),
            json!({"SetShieldMode":{"channel":"room","active":true}}),
            json!({"UpdateRewardRedemptionStatus":{"channel":"room","reward_id":"reward","redemption_id":"redemption","status":"FULFILLED","user_login":"viewer","reward_title":"title"}}),
            json!({"CreatePoll":{"channel":"room","title":"title","choices":["a","b"],"duration_secs":60,"channel_points_per_vote":null}}),
            json!({"EndPoll":{"channel":"room","status":"ARCHIVED"}}),
            json!({"CreatePrediction":{"channel":"room","title":"title","outcomes":["a","b"],"duration_secs":60}}),
            json!({"LockPrediction":{"channel":"room"}}),
            json!({"ResolvePrediction":{"channel":"room","winning_outcome_index":1}}),
            json!({"CancelPrediction":{"channel":"room"}}),
            json!({"StartCommercial":{"channel":"room","length_secs":30}}),
            json!({"CreateStreamMarker":{"channel":"room","description":null}}),
            json!({"SendAnnouncement":{"channel":"room","message":"announcement","color":null}}),
            json!({"SendShoutout":{"channel":"room","target_login":"viewer"}}),
            json!({"UpdateChannelInfo":{"channel":"room","title":"title","game_name":null}}),
        ];
        let requests = AuthValidationRequests::default();
        for value in commands {
            let command: AppCommand = serde_json::from_value(value).unwrap();
            assert!(
                matches!(
                    requests.action_rejection(&command, false, false),
                    Some(AppEvent::Error { .. })
                ),
                "unguarded account action: {command:?}"
            );
            assert!(requests.action_rejection(&command, true, false).is_none());
        }
    }

    #[test]
    fn same_account_refresh_and_local_commands_remain_usable() {
        let mut requests = AuthValidationRequests::default();
        requests.begin(ValidationKind::Refresh, Some("alice".into()));
        assert!(requests
            .action_rejection(&chat_command(), true, true)
            .is_none());
        requests.begin(ValidationKind::SwitchAccount, Some("bob".into()));
        let channel = crust_core::ChannelId::new("room");
        for command in [
            AppCommand::JoinChannel {
                channel: channel.clone(),
            },
            AppCommand::ClearLocalMessages {
                channel: channel.clone(),
            },
            AppCommand::InjectLocalMessage {
                channel,
                text: "local help".into(),
            },
            AppCommand::SendMessage {
                channel: crust_core::ChannelId::irc("irc.example", 6697, true, "#room"),
                text: "independent IRC account".into(),
                reply_to_msg_id: None,
                reply: None,
            },
            AppCommand::Logout,
        ] {
            assert!(requests.action_rejection(&command, false, false).is_none());
        }
    }

    #[test]
    fn logout_discards_pending_startup_login_add_and_refresh_results() {
        for kind in [
            ValidationKind::Startup,
            ValidationKind::Login,
            ValidationKind::AddAccount,
            ValidationKind::SwitchAccount,
            ValidationKind::Refresh,
        ] {
            let mut requests = AuthValidationRequests::default();
            let request = requests.begin(kind, None);
            requests.cancel();
            assert!(requests.finish(validated(request, "alice")).is_none());
            assert!(!requests.is_pending());
        }
    }

    #[test]
    fn late_refresh_cannot_overwrite_the_selected_accounts_token() {
        let mut requests = AuthValidationRequests::default();
        let refresh = requests.begin(ValidationKind::Refresh, Some("alice".into()));
        let switch = requests.begin(ValidationKind::SwitchAccount, Some("bob".into()));
        let mut settings = AppSettings::default();
        let alice = validated(refresh.clone(), "alice");
        save_validated_account(&mut settings, &alice.token, alice.result.as_ref().unwrap());
        assert!(requests.finish(alice).is_none());
        assert!(requests.is_pending());
        let bob = requests.finish(validated(switch, "bob")).unwrap();
        save_validated_account(&mut settings, &bob.token, bob.result.as_ref().unwrap());
        assert!(requests.finish(validated(refresh, "alice")).is_none());
        assert_eq!(settings.username, "bob");
        assert_eq!(settings.oauth_token, "bob-token");
        assert_eq!(settings.accounts[0].oauth_token, "alice-token");
        assert_eq!(settings.accounts[1].oauth_token, "bob-token");
    }

    #[test]
    fn a_newer_login_wins_even_when_the_older_request_finishes_last() {
        let mut requests = AuthValidationRequests::default();
        let old = requests.begin(ValidationKind::Login, None);
        let new = requests.begin(ValidationKind::Login, None);
        let completion = requests.finish(validated(new, "bob")).unwrap();
        let info = completion.result.unwrap();
        assert_eq!(info.client_id, "bob-client");
        assert_eq!(info.scopes, ["bob-scope"]);
        assert!(requests.finish(validated(old, "alice")).is_none());
    }

    #[test]
    fn saved_account_validation_rejects_another_tokens_owner() {
        for kind in [ValidationKind::SwitchAccount, ValidationKind::Refresh] {
            let mut requests = AuthValidationRequests::default();
            let request = requests.begin(kind, Some("bob".into()));
            let result = requests.finish(validated(request, "alice")).unwrap();
            assert!(matches!(
                result.result,
                Err(ValidateError::WrongAccount { expected, actual })
                    if expected == "bob" && actual == "alice"
            ));
        }
    }

    #[test]
    fn saved_identity_matches_case_insensitively_and_updates_one_account() {
        let mut requests = AuthValidationRequests::default();
        let request = requests.begin(ValidationKind::SwitchAccount, Some("ALICE".into()));
        let result = requests.finish(validated(request, "alice")).unwrap();
        let mut settings = AppSettings::default();
        settings.accounts.push(crust_storage::AccountEntry {
            username: "ALICE".into(),
            oauth_token: "expired-token".into(),
        });
        save_validated_account(
            &mut settings,
            &result.token,
            result.result.as_ref().unwrap(),
        );
        assert_eq!(settings.accounts.len(), 1);
        assert_eq!(settings.accounts[0].username, "alice");
        assert_eq!(settings.accounts[0].oauth_token, "alice-token");
    }

    async fn validation_server(
        response: Option<&'static str>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let size = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]).to_ascii_lowercase();
            assert!(request.contains("authorization: oauth test-token"));
            if let Some(response) = response {
                socket.write_all(response.as_bytes()).await.unwrap();
            } else {
                std::future::pending::<()>().await;
            }
        });
        (format!("http://{address}/validate"), task)
    }

    #[tokio::test]
    async fn token_validation_has_a_deadline() {
        let (url, server) = validation_server(None).await;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            validate_token_at("oauth:test-token", &url, Duration::from_millis(100)),
        )
        .await
        .expect("token validation must not wait indefinitely");
        server.abort();
        assert!(matches!(result, Err(ValidateError::Transient(_))));
    }

    #[tokio::test]
    async fn account_changes_clear_roles_and_cosmetics_after_signing_the_ui_out() {
        use crust_core::{events::AppEvent, model::ChannelId};
        use std::collections::HashMap;

        let channel = ChannelId::new("room");
        let mut roles = HashMap::from([(channel.clone(), true)]);
        let mut badges = HashMap::from([(channel.clone(), Vec::new())]);
        let mut color = Some("#123456".to_owned());
        let (tx, mut events) = mpsc::channel(4);
        crate::clear_account_channel_state(&mut roles, &mut badges, &mut color, &tx).await;
        assert!(roles.is_empty());
        assert!(badges.is_empty());
        assert!(color.is_none());
        assert!(matches!(events.try_recv(), Ok(AppEvent::LoggedOut)));
        assert!(matches!(events.try_recv(), Ok(AppEvent::UserStateUpdated {
            channel: updated, is_mod: false, badges, color: None,
        }) if updated == channel && badges.is_empty()));
    }

    #[tokio::test]
    async fn revoked_tokens_are_distinct_from_malformed_identity_responses() {
        let (url, server) = validation_server(Some(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ))
        .await;
        assert!(matches!(
            validate_token_at("test-token", &url, Duration::from_secs(1)).await,
            Err(ValidateError::Unauthorized)
        ));
        server.await.unwrap();
        let (url, server) = validation_server(Some(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        ))
        .await;
        assert!(matches!(
            validate_token_at("test-token", &url, Duration::from_secs(1)).await,
            Err(ValidateError::Transient(_))
        ));
        server.await.unwrap();
    }
}
