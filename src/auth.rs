//! Microsoft account -> Xbox Live -> xHome streaming token.
//!
//! Device-code sign-in: the user approves on microsoft.com/link, we only ever
//! hold OAuth tokens. Only the refresh token is persisted (see `store`).

use crate::store;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const CLIENT_ID: &str = "1f907974-e22b-4810-a9de-d9647380c97e";
const SCOPE: &str = "xboxlive.signin openid profile offline_access";
const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const TRANSFER_SCOPE: &str =
    "service::http://Passport.NET/purpose::PURPOSE_XBOX_CLOUD_CONSOLE_TRANSFER_TOKEN";

#[derive(Debug, Deserialize)]
pub struct DeviceCode {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    #[serde(default)]
    pub interval: u64,
}

// No `Debug`: these must never end up in a log.
#[derive(Deserialize)]
struct OAuthToken {
    access_token: String,
    refresh_token: String,
}

/// There is no usable sign-in: none was ever stored, or Microsoft no longer
/// accepts the stored one. The answer is to sign in again, not to retry.
#[derive(Debug)]
pub struct NotSignedIn;

impl std::fmt::Display for NotSignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not signed in - run `mestream login` first")
    }
}

impl std::error::Error for NotSignedIn {}

fn stored_refresh_token() -> Result<String> {
    store::load_refresh_token()?.ok_or_else(|| NotSignedIn.into())
}

/// Everything needed to talk to the xHome streaming service.
pub struct Session {
    pub gs_token: String,
    pub base_uri: String,
    refresh_token: String,
}

async fn json_or_error(resp: reqwest::Response, what: &str) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{what} failed: HTTP {status}: {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("{what}: invalid JSON"))
}

pub async fn begin_login(http: &reqwest::Client) -> Result<DeviceCode> {
    let resp = http
        .post(DEVICE_CODE_URL)
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await?;
    Ok(serde_json::from_value(json_or_error(resp, "device code request").await?)?)
}

/// The sign-in code ran out before anyone approved it: ask for a new one.
#[derive(Debug)]
pub struct CodeExpired;

impl std::fmt::Display for CodeExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the sign-in code expired before it was approved")
    }
}

impl std::error::Error for CodeExpired {}

/// Polls until the user has approved the device code, then stores the refresh token.
pub async fn finish_login(http: &reqwest::Client, code: &DeviceCode) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = Duration::from_secs(code.interval.max(1));
    loop {
        tokio::time::sleep(interval).await;
        if Instant::now() > deadline {
            return Err(CodeExpired.into());
        }
        let resp = http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", code.device_code.as_str()),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await.context("token poll: invalid JSON")?;
        if status.is_success() {
            let token: OAuthToken = serde_json::from_value(body)?;
            store::save_refresh_token(&token.refresh_token)?;
            return Ok(());
        }
        match body["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            Some("expired_token") => return Err(CodeExpired.into()),
            _ => bail!("sign-in failed: {body}"),
        }
    }
}

async fn refresh(http: &reqwest::Client, refresh_token: &str) -> Result<OAuthToken> {
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", SCOPE),
        ])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::BAD_REQUEST && text.contains("invalid_grant") {
        // Revoked or expired: keeping it would only fail the same way again.
        store::clear()?;
        return Err(NotSignedIn.into());
    }
    if !status.is_success() {
        bail!("token refresh failed: HTTP {status}: {text}");
    }
    let token: OAuthToken = serde_json::from_str(&text).context("token refresh: invalid JSON")?;
    store::save_refresh_token(&token.refresh_token)?;
    Ok(token)
}

async fn xbox_token(http: &reqwest::Client, url: &str, body: Value, what: &str) -> Result<String> {
    let resp = http
        .post(url)
        .header("x-xbl-contract-version", "1")
        .header("Cache-Control", "no-cache")
        .header("Origin", "https://www.xbox.com")
        .header("Referer", "https://www.xbox.com/")
        .json(&body)
        .send()
        .await?;
    let v = json_or_error(resp, what).await?;
    v["Token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{what}: response has no Token"))
}

/// Trades a Microsoft access token for an Xbox Live user token.
async fn xbox_user_token(http: &reqwest::Client, access_token: &str) -> Result<String> {
    xbox_token(
        http,
        "https://user.auth.xboxlive.com/user/authenticate",
        json!({
            "Properties": {
                "AuthMethod": "RPS",
                "RpsTicket": format!("d={access_token}"),
                "SiteName": "user.auth.xboxlive.com",
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        }),
        "Xbox user authentication",
    )
    .await
}

/// Signs in with the stored refresh token and returns a streaming session token.
pub async fn sign_in(http: &reqwest::Client) -> Result<Session> {
    let oauth = refresh(http, &stored_refresh_token()?).await?;
    let user_token = xbox_user_token(http, &oauth.access_token).await?;

    let gssv_token = xbox_token(
        http,
        "https://xsts.auth.xboxlive.com/xsts/authorize",
        json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [user_token] },
            "RelyingParty": "http://gssv.xboxlive.com/",
            "TokenType": "JWT",
        }),
        "XSTS authorization",
    )
    .await?;

    let resp = http
        .post("https://xhome.gssv-play-prod.xboxlive.com/v2/login/user")
        .header("x-gssv-client", "XboxComBrowser")
        .header("Cache-Control", "no-store, must-revalidate, no-cache")
        .json(&json!({ "token": gssv_token, "offeringId": "xhome" }))
        .send()
        .await?;
    let v = json_or_error(resp, "streaming login").await?;
    let gs_token = v["gsToken"]
        .as_str()
        .ok_or_else(|| anyhow!("streaming login: no gsToken"))?
        .to_owned();
    let regions = v["offeringSettings"]["regions"]
        .as_array()
        .ok_or_else(|| anyhow!("streaming login: no regions"))?;
    let region = regions
        .iter()
        .find(|r| r["isDefault"].as_bool() == Some(true))
        .or_else(|| regions.first())
        .ok_or_else(|| anyhow!("streaming login: empty region list"))?;
    let base_uri = region["baseUri"]
        .as_str()
        .ok_or_else(|| anyhow!("streaming login: region has no baseUri"))?
        .trim_end_matches('/')
        .to_owned();
    if !is_xbox_service(&base_uri) {
        bail!("streaming login: the service address is not an Xbox one");
    }

    Ok(Session { gs_token, base_uri, refresh_token: oauth.refresh_token })
}

/// The streaming address comes from a server answer, and our tokens are sent
/// to it: accept nothing but an encrypted address on Xbox's own domain.
fn is_xbox_service(address: &str) -> bool {
    let Some(rest) = address.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // A user name or port in front of the host could disguise where it leads.
    !host.contains(['@', ':']) && (host == "xboxlive.com" || host.ends_with(".xboxlive.com"))
}

impl Session {
    /// Short-lived token the console asks for when a session reaches `ReadyToConnect`.
    pub async fn transfer_token(&self, http: &reqwest::Client) -> Result<String> {
        let resp = http
            .post("https://login.live.com/oauth20_token.srf")
            .form(&[
                ("client_id", CLIENT_ID),
                ("scope", TRANSFER_SCOPE),
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh_token.as_str()),
            ])
            .send()
            .await?;
        let v = json_or_error(resp, "console transfer token").await?;
        v["access_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("console transfer token: no access_token"))
    }
}

/// `Authorization` header for the Xbox console-management API (the service
/// behind the Xbox app's remote control of a console).
pub async fn console_api_authorization(http: &reqwest::Client) -> Result<String> {
    let oauth = refresh(http, &stored_refresh_token()?).await?;
    let user_token = xbox_user_token(http, &oauth.access_token).await?;
    let resp = http
        .post("https://xsts.auth.xboxlive.com/xsts/authorize")
        .header("x-xbl-contract-version", "1")
        .json(&json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [user_token] },
            "RelyingParty": "http://xboxlive.com",
            "TokenType": "JWT",
        }))
        .send()
        .await?;
    let v = json_or_error(resp, "XSTS authorization").await?;
    let token = v["Token"].as_str().ok_or_else(|| anyhow!("XSTS authorization: no Token"))?;
    let user_hash = v["DisplayClaims"]["xui"][0]["uhs"]
        .as_str()
        .ok_or_else(|| anyhow!("XSTS authorization: no user hash"))?;
    Ok(format!("XBL3.0 x={user_hash};{token}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_only_go_to_encrypted_xbox_addresses() {
        assert!(is_xbox_service("https://weu.core.gssv-play-prodxhome.xboxlive.com"));
        assert!(is_xbox_service("https://xboxlive.com/path"));
        assert!(!is_xbox_service("http://weu.gssv.xboxlive.com"));
        assert!(!is_xbox_service("https://xboxlive.com.example.org"));
        assert!(!is_xbox_service("https://example.org/.xboxlive.com"));
        assert!(!is_xbox_service("https://xboxlive.com@example.org"));
        assert!(!is_xbox_service("https://notxboxlive.com"));
    }
}
