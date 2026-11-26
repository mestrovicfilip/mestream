//! xHome (console remote play) session API: list consoles, start a session,
//! exchange SDP and ICE with the console.

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Console {
    pub device_name: String,
    pub server_id: String,
    #[serde(default)]
    pub power_state: String,
    #[serde(default)]
    pub console_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    P720,
    P1080,
}

impl Resolution {
    pub fn size(self) -> (u32, u32) {
        match self {
            Resolution::P720 => (1280, 720),
            Resolution::P1080 => (1920, 1080),
        }
    }
    /// The console picks its encode resolution from the OS the client claims to be.
    fn os_name(self) -> &'static str {
        match self {
            Resolution::P720 => "android",
            Resolution::P1080 => "windows",
        }
    }
}

/// How far this machine's clock is ahead of UTC right now.
fn utc_offset_minutes() -> i32 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut zone = TIME_ZONE_INFORMATION::default();
    // 1 = standard time is in effect, 2 = daylight saving time.
    let seasonal = match unsafe { GetTimeZoneInformation(&mut zone) } {
        1 => zone.StandardBias,
        2 => zone.DaylightBias,
        _ => 0,
    };
    // Windows counts the other way: minutes to add to local time to get UTC.
    -(zone.Bias + seasonal)
}

pub struct XHome {
    http: reqwest::Client,
    base: String,
    token: String,
    resolution: Resolution,
}

pub struct StreamSession<'a> {
    api: &'a XHome,
    path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCandidate {
    pub candidate: String,
    #[serde(default)]
    pub sdp_mid: Option<String>,
    #[serde(default, rename = "sdpMLineIndex")]
    pub sdp_mline_index: Option<u16>,
}

impl XHome {
    pub fn new(http: reqwest::Client, base: String, token: String, resolution: Resolution) -> Self {
        Self { http, base, token, resolution }
    }

    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    fn device_info(&self) -> String {
        let (w, h) = self.resolution.size();
        json!({
            "appInfo": { "env": {
                "clientAppId": "www.xbox.com",
                "clientAppType": "browser",
                "clientAppVersion": "21.1.98",
                "clientSdkVersion": "8.5.3",
                "httpEnvironment": "prod",
                "sdkInstallId": "",
            }},
            "dev": {
                "hw": { "make": "Microsoft", "model": "unknown", "sdktype": "web" },
                "os": { "name": self.resolution.os_name(), "ver": "22631.2715", "platform": "desktop" },
                "displayInfo": {
                    "dimensions": { "widthInPixels": w, "heightInPixels": h },
                    "pixelDensity": { "dpiX": 1, "dpiY": 1 },
                },
                "browser": { "browserName": "chrome", "browserVersion": "119.0" },
            },
        })
        .to_string()
    }

    async fn request(&self, method: Method, path: &str, body: Option<String>) -> Result<(StatusCode, String)> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let mut req = self
            .http
            .request(method, &url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("X-Gssv-Client", "XboxComBrowser")
            .header("X-MS-Device-Info", self.device_info())
            .bearer_auth(&self.token);
        if let Some(body) = body {
            req = req.body(body);
        }
        let resp = req.send().await.with_context(|| format!("request to {path}"))?;
        let status = resp.status();
        Ok((status, resp.text().await.unwrap_or_default()))
    }

    async fn request_ok(&self, method: Method, path: &str, body: Option<String>) -> Result<String> {
        let (status, text) = self.request(method, path, body).await?;
        if !status.is_success() {
            bail!("{path}: HTTP {status}: {text}");
        }
        Ok(text)
    }

    /// Closes a session by its path (used for one left over from an earlier launch).
    pub async fn delete_session(&self, path: &str) -> Result<()> {
        self.request_ok(Method::DELETE, path, None).await.map(|_| ())
    }

    pub async fn consoles(&self) -> Result<Vec<Console>> {
        let text = self.request_ok(Method::GET, "v6/servers/home", None).await?;
        let v: Value = serde_json::from_str(&text)?;
        serde_json::from_value(v["results"].clone()).context("unexpected console list format")
    }

    pub async fn start(&self, server_id: &str) -> Result<StreamSession<'_>> {
        let body = json!({
            "clientSessionId": "",
            "titleId": "",
            "systemUpdateGroup": "",
            "settings": {
                "nanoVersion": "V3;WebrtcTransport.dll",
                "enableOptionalDataCollection": false,
                "enableTextToSpeech": false,
                "highContrast": 0,
                "locale": "en-US",
                "useIceConnection": false,
                "timezoneOffsetMinutes": utc_offset_minutes(),
                "sdkType": "web",
                "osName": self.resolution.os_name(),
            },
            "serverId": server_id,
            "fallbackRegionNames": [],
        });
        let text = self
            .request_ok(Method::POST, "v5/sessions/home/play", Some(body.to_string()))
            .await?;
        let v: Value = serde_json::from_str(&text)?;
        let path = v["sessionPath"]
            .as_str()
            .ok_or_else(|| anyhow!("session start: no sessionPath in {text}"))?
            .to_owned();
        Ok(StreamSession { api: self, path })
    }
}

impl StreamSession<'_> {
    pub fn path(&self) -> &str {
        &self.path
    }

    fn sub(&self, suffix: &str) -> String {
        format!("{}/{}", self.path, suffix)
    }

    /// Waits until the console is provisioned. `transfer_token` is fetched lazily
    /// because the console only asks for it when it needs the user signed in.
    pub async fn wait_provisioned<F, Fut>(&self, transfer_token: F) -> Result<()>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut sent_connect = false;
        let mut last = String::new();
        loop {
            let text = self.api.request_ok(Method::GET, &self.sub("state"), None).await?;
            let v: Value = serde_json::from_str(&text)?;
            let state = v["state"].as_str().unwrap_or_default().to_owned();
            if state != last {
                crate::status!("[session] state: {state}");
                last = state.clone();
            }
            match state.as_str() {
                "Provisioned" => return Ok(()),
                "ReadyToConnect" if !sent_connect => {
                    let token = transfer_token().await?;
                    self.api
                        .request_ok(
                            Method::POST,
                            &self.sub("connect"),
                            Some(json!({ "userToken": token }).to_string()),
                        )
                        .await?;
                    sent_connect = true;
                }
                "Failed" => bail!("console refused the session: {text}"),
                _ => {}
            }
            if Instant::now() > deadline {
                bail!("timed out waiting for the console (last state: {state})");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn poll_exchange(&self, suffix: &str) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (status, text) = self.api.request(Method::GET, &self.sub(suffix), None).await?;
            if status == StatusCode::OK && !text.is_empty() {
                let v: Value = serde_json::from_str(&text)?;
                let inner = v["exchangeResponse"]
                    .as_str()
                    .ok_or_else(|| anyhow!("{suffix}: no exchangeResponse in {text}"))?;
                return serde_json::from_str(inner).with_context(|| format!("{suffix}: bad exchangeResponse"));
            }
            if !status.is_success() {
                bail!("{suffix}: HTTP {status}: {text}");
            }
            if Instant::now() > deadline {
                bail!("timed out waiting for the console's {suffix} answer");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Sends our SDP offer and returns the console's SDP answer.
    pub async fn exchange_sdp(&self, offer_sdp: &str) -> Result<String> {
        let body = json!({
            "messageType": "offer",
            "sdp": offer_sdp,
            "requestId": "1",
            "configuration": {
                "chatConfiguration": {
                    "bytesPerSample": 2,
                    "expectedClipDurationMs": 20,
                    "format": { "codec": "opus", "container": "webm" },
                    "numChannels": 1,
                    "sampleFrequencyHz": 24000,
                },
                "chat": { "minVersion": 1, "maxVersion": 1 },
                "control": { "minVersion": 1, "maxVersion": 3 },
                "input": { "minVersion": 1, "maxVersion": 9 },
                "message": { "minVersion": 1, "maxVersion": 1 },
                "reliableinput": { "minVersion": 9, "maxVersion": 9 },
                "unreliableinput": { "minVersion": 9, "maxVersion": 9 },
            },
        });
        self.api
            .request_ok(Method::POST, &self.sub("sdp"), Some(body.to_string()))
            .await?;
        let answer = self.poll_exchange("sdp").await?;
        answer["sdp"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("SDP answer has no sdp field: {answer}"))
    }

    /// Sends our ICE candidates (already JSON-encoded, one string each) and
    /// returns the console's candidates.
    pub async fn exchange_ice(&self, candidates: Vec<String>) -> Result<Vec<RemoteCandidate>> {
        self.api
            .request_ok(
                Method::POST,
                &self.sub("ice"),
                Some(json!({ "candidates": candidates }).to_string()),
            )
            .await?;
        let v = self.poll_exchange("ice").await?;
        serde_json::from_value(v).context("unexpected ICE answer format")
    }

    pub async fn keepalive(&self) -> Result<()> {
        self.api
            .request_ok(Method::POST, &self.sub("keepalive"), Some(String::new()))
            .await
            .map(|_| ())
    }

    pub async fn stop(&self) -> Result<()> {
        self.api.request_ok(Method::DELETE, &self.path, None).await.map(|_| ())
    }
}
