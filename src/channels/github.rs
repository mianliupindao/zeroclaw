use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

const GITHUB_API_BASE: &str = "https://api.github.com";
const GITHUB_USER_AGENT: &str = "zeroclaw-github-channel";

/// GitHub channel in webhook mode.
///
/// Incoming messages are received by gateway endpoint `/github`.
/// Outbound replies are sent as issue/PR comments via GitHub REST API.
pub struct GitHubChannel {
    api_token: String,
    api_base_url: String,
    webhook_secret: Option<String>,
    allowed_repos: Vec<String>,
    allowed_users: Vec<String>,
    bot_login: Option<String>,
    client: reqwest::Client,
}

impl GitHubChannel {
    pub fn new(
        api_token: String,
        api_base_url: String,
        webhook_secret: Option<String>,
        allowed_repos: Vec<String>,
        allowed_users: Vec<String>,
        bot_login: Option<String>,
    ) -> Self {
        Self {
            api_token: api_token.trim().to_string(),
            api_base_url: api_base_url.trim_end_matches('/').to_string(),
            webhook_secret: webhook_secret.map(|value| value.trim().to_string()),
            allowed_repos,
            allowed_users,
            bot_login: bot_login.map(|value| value.trim().to_string()),
            client: crate::config::build_runtime_proxy_client("channel.github"),
        }
    }

    pub fn webhook_secret(&self) -> Option<&str> {
        self.webhook_secret
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn api_base_url(&self) -> &str {
        if self.api_base_url.trim().is_empty() {
            GITHUB_API_BASE
        } else {
            self.api_base_url.as_str()
        }
    }

    fn allowlist_matches(allowlist: &[String], value: &str) -> bool {
        if allowlist.is_empty() {
            return false;
        }

        allowlist.iter().any(|entry| {
            let entry = entry.trim();
            !entry.is_empty() && (entry == "*" || entry.eq_ignore_ascii_case(value))
        })
    }

    fn is_repo_allowed(&self, repo: &str) -> bool {
        Self::allowlist_matches(&self.allowed_repos, repo)
    }

    fn is_user_allowed(&self, login: &str) -> bool {
        Self::allowlist_matches(&self.allowed_users, login)
    }

    fn is_self_message(&self, login: &str) -> bool {
        self.bot_login
            .as_deref()
            .map(str::trim)
            .is_some_and(|bot| !bot.is_empty() && bot.eq_ignore_ascii_case(login))
    }

    fn now_unix_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn parse_timestamp_secs(value: Option<&serde_json::Value>) -> u64 {
        let Some(raw) = value.and_then(|v| v.as_str()) else {
            return Self::now_unix_secs();
        };

        chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|dt| dt.timestamp().unsigned_abs())
            .unwrap_or_else(Self::now_unix_secs)
    }

    fn build_message(
        &self,
        channel: &str,
        repo: &str,
        issue_or_pr_number: u64,
        sender: &str,
        content: &str,
        comment_id: Option<u64>,
        created_at: Option<&serde_json::Value>,
    ) -> Option<ChannelMessage> {
        let trimmed_content = content.trim();
        if trimmed_content.is_empty() {
            return None;
        }

        if !self.is_repo_allowed(repo) {
            tracing::warn!(
                "GitHub: ignoring webhook from unauthorized repository: {repo}. \
                Add to channels.github.allowed_repos in config.toml, or use \"*\" for all."
            );
            return None;
        }

        if !self.is_user_allowed(sender) {
            tracing::warn!(
                "GitHub: ignoring webhook from unauthorized sender: {sender}. \
                Add to channels.github.allowed_users in config.toml, or use \"*\" for all."
            );
            return None;
        }

        if self.is_self_message(sender) {
            tracing::debug!("GitHub: skipping self-authored comment from {sender}");
            return None;
        }

        let comment_id = comment_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let reply_target = format!("{repo}#{issue_or_pr_number}");

        Some(ChannelMessage {
            id: comment_id.clone(),
            sender: sender.to_string(),
            reply_target,
            content: trimmed_content.to_string(),
            channel: channel.to_string(),
            timestamp: Self::parse_timestamp_secs(created_at),
            thread_ts: Some(comment_id),
        })
    }

    fn parse_issue_comment(&self, payload: &serde_json::Value) -> Option<ChannelMessage> {
        let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");
        if action != "created" {
            return None;
        }

        let repo = payload
            .get("repository")
            .and_then(|repo| repo.get("full_name"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())?;
        let issue_number = payload
            .get("issue")
            .and_then(|issue| issue.get("number"))
            .and_then(|v| v.as_u64())?;
        let sender = payload
            .get("comment")
            .and_then(|comment| comment.get("user"))
            .and_then(|user| user.get("login"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())?;
        let body = payload
            .get("comment")
            .and_then(|comment| comment.get("body"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let comment_id = payload
            .get("comment")
            .and_then(|comment| comment.get("id"))
            .and_then(|v| v.as_u64());
        let created_at = payload
            .get("comment")
            .and_then(|comment| comment.get("created_at"));

        self.build_message(
            "github",
            repo,
            issue_number,
            sender,
            body,
            comment_id,
            created_at,
        )
    }

    fn parse_review_comment(&self, payload: &serde_json::Value) -> Option<ChannelMessage> {
        let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");
        if action != "created" {
            return None;
        }

        let repo = payload
            .get("repository")
            .and_then(|repo| repo.get("full_name"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())?;
        let pr_number = payload
            .get("pull_request")
            .and_then(|pr| pr.get("number"))
            .and_then(|v| v.as_u64())?;
        let sender = payload
            .get("comment")
            .and_then(|comment| comment.get("user"))
            .and_then(|user| user.get("login"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())?;
        let body = payload
            .get("comment")
            .and_then(|comment| comment.get("body"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let comment_id = payload
            .get("comment")
            .and_then(|comment| comment.get("id"))
            .and_then(|v| v.as_u64());
        let created_at = payload
            .get("comment")
            .and_then(|comment| comment.get("created_at"));

        self.build_message(
            "github", repo, pr_number, sender, body, comment_id, created_at,
        )
    }

    pub fn parse_webhook_payload(
        &self,
        event: &str,
        payload: &serde_json::Value,
    ) -> Vec<ChannelMessage> {
        let mut messages = Vec::new();

        let parsed = match event {
            "issue_comment" => self.parse_issue_comment(payload),
            "pull_request_review_comment" => self.parse_review_comment(payload),
            _ => None,
        };

        if let Some(message) = parsed {
            messages.push(message);
        }

        messages
    }

    fn parse_reply_target(reply_target: &str) -> Option<(&str, u64)> {
        let (repo, issue_or_pr) = reply_target.rsplit_once('#')?;
        let repo = repo.trim();
        if repo.is_empty() {
            return None;
        }
        let issue_or_pr = issue_or_pr.trim().parse::<u64>().ok()?;
        Some((repo, issue_or_pr))
    }
}

#[async_trait]
impl Channel for GitHubChannel {
    fn name(&self) -> &str {
        "github"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let Some((repo, issue_or_pr_number)) = Self::parse_reply_target(&message.recipient) else {
            anyhow::bail!(
                "GitHub send target is invalid: expected owner/repo#<issue_or_pr_number>, got {}",
                message.recipient
            );
        };

        let url = format!(
            "{}/repos/{repo}/issues/{issue_or_pr_number}/comments",
            self.api_base_url()
        );

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", GITHUB_USER_AGENT)
            .json(&serde_json::json!({ "body": message.content }))
            .send()
            .await?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let sanitized = crate::providers::sanitize_api_error(&body);
        tracing::error!("GitHub send failed: {status} — {sanitized}");
        anyhow::bail!("GitHub API error: {status}");
    }

    async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        tracing::info!(
            "GitHub channel active (webhook mode). \
            Configure GitHub webhook to POST to your gateway's /github endpoint."
        );
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }

    async fn health_check(&self) -> bool {
        let url = format!("{}/rate_limit", self.api_base_url());
        self.client
            .get(url)
            .bearer_auth(&self.api_token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", GITHUB_USER_AGENT)
            .send()
            .await
            .map(|response| response.status().is_success())
            .unwrap_or(false)
    }
}

/// Verify `X-Hub-Signature-256` for GitHub webhooks.
///
/// Signature format: `sha256=<hex(hmac_sha256(secret, raw_body_bytes))>`.
pub fn verify_github_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let signature = signature_header
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or(signature_header)
        .trim();
    let Ok(provided) = hex::decode(signature) else {
        tracing::warn!("GitHub: invalid webhook signature format");
        return false;
    };

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel() -> GitHubChannel {
        GitHubChannel::new(
            "token".into(),
            GITHUB_API_BASE.into(),
            Some("secret".into()),
            vec!["zeroclaw-labs/zeroclaw".into()],
            vec!["theonlyhennygod".into()],
            Some("zeroclaw-bot".into()),
        )
    }

    #[test]
    fn github_channel_name() {
        let channel = make_channel();
        assert_eq!(channel.name(), "github");
    }

    #[test]
    fn parse_issue_comment_created_event() {
        let channel = make_channel();
        let payload = serde_json::json!({
            "action": "created",
            "repository": {"full_name": "zeroclaw-labs/zeroclaw"},
            "issue": {"number": 2079},
            "comment": {
                "id": 12345,
                "body": "please add this",
                "created_at": "2026-02-27T12:00:00Z",
                "user": {"login": "theonlyhennygod"}
            }
        });

        let messages = channel.parse_webhook_payload("issue_comment", &payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].reply_target, "zeroclaw-labs/zeroclaw#2079");
        assert_eq!(messages[0].sender, "theonlyhennygod");
        assert_eq!(messages[0].content, "please add this");
        assert_eq!(messages[0].id, "12345");
        assert_eq!(messages[0].channel, "github");
    }

    #[test]
    fn parse_review_comment_created_event() {
        let channel = make_channel();
        let payload = serde_json::json!({
            "action": "created",
            "repository": {"full_name": "zeroclaw-labs/zeroclaw"},
            "pull_request": {"number": 2195},
            "comment": {
                "id": 999,
                "body": "nit: rename this",
                "created_at": "2026-02-27T12:00:00Z",
                "user": {"login": "theonlyhennygod"}
            }
        });

        let messages = channel.parse_webhook_payload("pull_request_review_comment", &payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].reply_target, "zeroclaw-labs/zeroclaw#2195");
        assert_eq!(messages[0].channel, "github");
    }

    #[test]
    fn parse_webhook_ignores_unauthorized_or_self_sender() {
        let channel = make_channel();

        let unauthorized_user = serde_json::json!({
            "action": "created",
            "repository": {"full_name": "zeroclaw-labs/zeroclaw"},
            "issue": {"number": 1},
            "comment": {"id": 1, "body": "hello", "user": {"login": "someone-else"}}
        });
        assert!(channel
            .parse_webhook_payload("issue_comment", &unauthorized_user)
            .is_empty());

        let self_sender = serde_json::json!({
            "action": "created",
            "repository": {"full_name": "zeroclaw-labs/zeroclaw"},
            "issue": {"number": 1},
            "comment": {"id": 1, "body": "hello", "user": {"login": "zeroclaw-bot"}}
        });
        assert!(channel
            .parse_webhook_payload("issue_comment", &self_sender)
            .is_empty());
    }

    #[test]
    fn verify_signature_accepts_valid_signature() {
        let secret = "github-webhook-secret";
        let body = br#"{"action":"created"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        assert!(verify_github_signature(secret, body, &signature));
    }

    #[test]
    fn verify_signature_rejects_invalid_signature() {
        assert!(!verify_github_signature(
            "github-webhook-secret",
            br#"{"action":"created"}"#,
            "sha256=deadbeef"
        ));
    }

    #[test]
    fn parse_reply_target_requires_owner_repo_hash_number() {
        assert_eq!(
            GitHubChannel::parse_reply_target("zeroclaw-labs/zeroclaw#42"),
            Some(("zeroclaw-labs/zeroclaw", 42))
        );
        assert!(GitHubChannel::parse_reply_target("zeroclaw-labs/zeroclaw").is_none());
        assert!(GitHubChannel::parse_reply_target("zeroclaw-labs/zeroclaw#x").is_none());
    }
}
