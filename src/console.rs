//! Console management through the Xbox service the Xbox mobile app uses:
//! read a console's real state and send it power commands. This is separate
//! from the streaming service and keeps working when streaming is stuck.

use crate::auth;
use crate::text::text;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub struct ConsoleApi {
    http: reqwest::Client,
    authorization: String,
}

impl ConsoleApi {
    pub async fn connect(http: reqwest::Client) -> Result<Self> {
        let authorization = auth::console_api_authorization(&http).await?;
        Ok(Self { http, authorization })
    }

    fn request(&self, method: reqwest::Method, path: &str, contract: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("https://xccs.xboxlive.com/{path}"))
            .header("Authorization", &self.authorization)
            .header("Accept-Language", "en-US")
            .header("skillplatform", "RemoteManagement")
            .header("x-xbl-contract-version", contract)
            .header("x-xbl-client-name", "XboxApp")
            .header("x-xbl-client-type", "UWA")
            .header("x-xbl-client-version", "39.39.22001.0")
    }

    async fn send(request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{what}: HTTP {status}: {text}");
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Every console registered to the account, as the service reports them.
    pub async fn devices(&self) -> Result<Vec<Value>> {
        let path = "lists/devices?queryCurrentDevice=false&includeStorageDevices=true";
        let list = Self::send(self.request(reqwest::Method::GET, path, "2"), "console list").await?;
        Ok(list["result"].as_array().cloned().unwrap_or_default())
    }

    /// Live state of one console (the service asks the console itself).
    pub async fn status(&self, console_id: &str) -> Result<Value> {
        // The id ends up in the address, so it must be nothing but an id.
        if !console_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("the console id has an unexpected form");
        }
        let path = format!("consoles/{console_id}");
        Self::send(self.request(reqwest::Method::GET, &path, "4"), "console status").await
    }

    /// Sends a power command: "WakeUp", "TurnOff" or "Reboot".
    pub async fn power(&self, console_id: &str, command: &str) -> Result<()> {
        let body = json!({
            "destination": "Xbox",
            "type": "Power",
            "command": command,
            "sessionId": uuid::Uuid::new_v4().to_string(),
            "sourceId": "com.microsoft.smartglass",
            "parameters": [{}],
            "linkedXboxId": console_id,
        });
        let reply = Self::send(
            self.request(reqwest::Method::POST, "commands", "4").json(&body),
            "power command",
        )
        .await?;
        let code = reply["status"]["errorCode"].as_str().unwrap_or("OK");
        if code == "ErrorCallingWNS" {
            // The same push channel the streaming service uses is down, so no
            // remote command can reach the console until it is restarted by hand.
            bail!("{}: {command} command failed: {}", text().console_deaf, reply["status"]);
        }
        if code != "OK" {
            bail!("the console refused the {command} command: {}", reply["status"]);
        }
        Ok(())
    }
}

/// The console the user plays on, among those the service lists. With no
/// choice recorded the only console is taken; a guess between several would
/// risk acting on the wrong one.
fn chosen<'a>(devices: &'a [Value], console_id: Option<&str>) -> Result<&'a Value> {
    match console_id {
        Some(id) => devices
            .iter()
            .find(|device| device["id"].as_str() == Some(id))
            .ok_or_else(|| anyhow!("the chosen console is not registered to this account")),
        None if devices.len() == 1 => Ok(&devices[0]),
        None if devices.is_empty() => Err(anyhow!("no console is registered to this account")),
        None => Err(anyhow!("there are several consoles on this account and none has been chosen")),
    }
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?)
}

/// Sleeps, or returns false at once if the window is closed meanwhile.
async fn pause(closed: &watch::Receiver<bool>, time: Duration) -> bool {
    let mut closed = closed.clone();
    tokio::select! {
        _ = tokio::time::sleep(time) => true,
        _ = closed.wait_for(|c| *c) => false,
    }
}

/// Restarts the console and waits until it is back and reachable.
pub async fn restart_and_wait(closed: &watch::Receiver<bool>, console_id: Option<&str>) -> Result<()> {
    let cancelled = || anyhow!("the window was closed");
    let api = ConsoleApi::connect(http_client()?).await?;
    let devices = api.devices().await?;
    let id = chosen(&devices, console_id)?["id"]
        .as_str()
        .ok_or_else(|| anyhow!("the console has no id"))?
        .to_owned();

    crate::status!("[console] sending restart");
    api.power(&id, "Reboot").await?;
    crate::status!("[console] restarting");
    // Give it time to actually go down before asking whether it is back.
    if !pause(closed, Duration::from_secs(25)).await {
        return Err(cancelled());
    }

    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let back = match api.status(&id).await {
            Ok(status) => {
                status["powerState"].as_str() == Some("On") && status["status"]["errorCode"].as_str() == Some("OK")
            }
            Err(_) => false,
        };
        if back {
            break;
        }
        if Instant::now() > deadline {
            bail!("the console did not come back within three minutes after the restart");
        }
        crate::status!("[console] waiting for the console to come back");
        if !pause(closed, Duration::from_secs(6)).await {
            return Err(cancelled());
        }
    }
    crate::status!("[console] back online, letting it settle");
    if !pause(closed, Duration::from_secs(12)).await {
        return Err(cancelled());
    }
    Ok(())
}

/// A plain-language reason the console cannot be streamed from, when its
/// settings or power state explain it. `None` when nothing stands out or the
/// service could not be asked.
pub async fn explain_failure(console_id: Option<&str>) -> Option<&'static str> {
    let api = ConsoleApi::connect(http_client().ok()?).await.ok()?;
    let devices = api.devices().await.ok()?;
    diagnosis(chosen(&devices, console_id).ok()?)
}

/// The service writes switches as true/false in some answers and as the
/// words "True"/"False" in others.
fn switched_off(value: &Value) -> bool {
    value.as_bool() == Some(false) || value.as_str().is_some_and(|word| word.eq_ignore_ascii_case("false"))
}

fn diagnosis(device: &Value) -> Option<&'static str> {
    let t = text();
    if switched_off(&device["consoleStreamingEnabled"]) {
        Some(t.remote_features_off)
    } else if device["powerState"].as_str() == Some("Off") {
        Some(t.console_off)
    } else {
        None
    }
}

/// Prints what the console-management service knows about each console.
pub async fn print_diagnosis() -> Result<()> {
    let api = ConsoleApi::connect(http_client()?).await?;
    let devices = api.devices().await?;
    if devices.is_empty() {
        println!("The Xbox service lists no consoles for this account.");
    }
    for device in devices {
        let field = |name: &str| device[name].to_string();
        println!("console: {} ({})", field("name"), field("consoleType"));
        println!("  powerState:               {}", field("powerState"));
        println!("  consoleStreamingEnabled:  {}", field("consoleStreamingEnabled"));
        println!("  remoteManagementEnabled:  {}", field("remoteManagementEnabled"));
        println!("  digitalAssistantRemoteControlEnabled: {}", field("digitalAssistantRemoteControlEnabled"));
        let Some(id) = device["id"].as_str() else { continue };
        match api.status(id).await {
            Ok(mut status) => {
                if let Some(object) = status.as_object_mut() {
                    object.remove("id");
                }
                println!("  live status: {status}");
            }
            Err(e) => println!("  live status: could not be read: {e:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str) -> Value {
        json!({ "id": id, "consoleStreamingEnabled": true, "powerState": "On" })
    }

    #[test]
    fn the_recorded_console_is_used_and_never_guessed_between_several() {
        let two = [device("AAA"), device("BBB")];
        assert_eq!(chosen(&two, Some("BBB")).unwrap()["id"], "BBB");
        assert!(chosen(&two, Some("CCC")).is_err());
        assert!(chosen(&two, None).is_err());
        assert_eq!(chosen(&two[..1], None).unwrap()["id"], "AAA");
        assert!(chosen(&[], None).is_err());
    }

    #[test]
    fn settings_and_power_state_explain_a_failure() {
        assert_eq!(diagnosis(&device("A")), None);
        let disabled = json!({ "consoleStreamingEnabled": false, "powerState": "On" });
        assert_eq!(diagnosis(&disabled), Some(text().remote_features_off));
        let disabled_in_words = json!({ "consoleStreamingEnabled": "False", "powerState": "On" });
        assert_eq!(diagnosis(&disabled_in_words), Some(text().remote_features_off));
        let enabled_in_words = json!({ "consoleStreamingEnabled": "True", "powerState": "On" });
        assert_eq!(diagnosis(&enabled_in_words), None);
        let off = json!({ "consoleStreamingEnabled": true, "powerState": "Off" });
        assert_eq!(diagnosis(&off), Some(text().console_off));
    }
}
