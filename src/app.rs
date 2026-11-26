//! What happens between starting Mestream and leaving it: sign in if needed,
//! choose a console if needed, stream, and after a failure offer the ways
//! out. Each step decides which step comes next.

use crate::options::PlayOptions;
use crate::overlay::{self, Action, Button, Prompt, Role, Style, Tone};
use crate::session::{self, Closed, Ended, Need, unless_closed};
use crate::text::{fill, text};
use crate::window::Window;
use crate::{auth, console, lifecycle, settings, store};
use anyhow::{Error, Result};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// After the stream drops (or the machine wakes from sleep) we reconnect by
/// ourselves this many times before bothering the user.
const RECONNECT_ATTEMPTS: u32 = 6;

enum Step {
    /// Connect and stream. The number says which attempt at getting a lost
    /// stream back this is; 0 for a connection the user asked for.
    Connect(u32),
    SignIn,
    ChooseConsole,
    NoConsole,
    RestartConsole,
    Failed(Error),
    Leave,
}

/// A connection the user asked for, as opposed to an automatic reconnect.
const FRESH: Step = Step::Connect(0);

/// Streams with a window (`play`) or without one (`probe`).
pub async fn run(options: PlayOptions) -> Result<()> {
    let (closed_tx, closed) = tokio::sync::watch::channel(false);
    let closed_tx = Arc::new(closed_tx);
    // Being ended from outside is handled like the window being closed.
    {
        let closed_tx = closed_tx.clone();
        tokio::spawn(async move {
            while !lifecycle::SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let _ = closed_tx.send(true);
        });
    }

    if !options.show {
        let ended = session::connect_and_stream(&options, None, &closed, &mut None).await?;
        return match ended {
            Ended::Lost => Err(anyhow::anyhow!("the stream was lost")),
            _ => Ok(()),
        };
    }

    release_own_console();
    let mut app = App { window: Window::open(&options, closed_tx), options, closed, account: None, renewed_sign_in: false };
    let mut step = FRESH;
    while !*app.closed.borrow() {
        step = match step {
            Step::Connect(attempt) => app.connect(attempt).await,
            Step::Leave => break,
            Step::SignIn => app.sign_in().await,
            Step::ChooseConsole => app.choose_console().await,
            Step::NoConsole => app.no_console().await,
            Step::RestartConsole => app.restart_console().await,
            Step::Failed(error) => app.failed(error).await,
        };
    }
    app.window.close()
}

/// When started from a shortcut we own the console window: let go of it so
/// only the stream window is on screen. Started from a terminal, keep it.
pub fn release_own_console() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    unsafe {
        let mut processes = [0u32; 2];
        if GetConsoleProcessList(&mut processes) == 1 {
            let _ = FreeConsole();
        }
    }
}

struct App {
    options: PlayOptions,
    window: Window,
    closed: Closed,
    /// The sign-in, kept so a reconnect does not have to repeat it.
    account: Option<auth::Session>,
    /// A rejected sign-in has already been renewed once since the last
    /// working stream; a second rejection is shown instead of retried.
    renewed_sign_in: bool,
}

impl App {
    async fn connect(&mut self, attempt: u32) -> Step {
        if attempt > 0 {
            overlay::begin_reconnect();
            crate::status!("[session] reconnecting ({attempt}/{RECONNECT_ATTEMPTS})");
            // The network needs a moment after waking up.
            let wait = Duration::from_secs(if attempt == 1 { 1 } else { 4 });
            if !self.pause(wait).await {
                return Step::Leave;
            }
        }
        let result =
            session::connect_and_stream(&self.options, Some(&mut self.window), &self.closed, &mut self.account).await;
        if result.is_ok() {
            self.renewed_sign_in = false;
        }
        match result {
            Ok(Ended::Finished) => Step::Leave,
            Ok(Ended::Lost) => Step::Connect(1),
            Ok(Ended::Requested(action)) => {
                overlay::begin_reconnect();
                self.step_for(action)
            }
            Err(error) => match self.step_after(error) {
                Step::Failed(_) if (1..RECONNECT_ATTEMPTS).contains(&attempt) => Step::Connect(attempt + 1),
                other => other,
            },
        }
    }

    /// Sleeps; false if the window was closed meanwhile.
    async fn pause(&self, time: Duration) -> bool {
        let mut closed = self.closed.clone();
        tokio::select! {
            _ = closed.wait_for(|c| *c) => false,
            _ = tokio::time::sleep(time) => true,
        }
    }

    /// Waits for the user to choose something on the prompt that is up.
    /// `None` when the window is closed instead.
    async fn next_action(&self) -> Option<Action> {
        loop {
            if let Some(action) = overlay::take_action() {
                return Some(action);
            }
            if !self.pause(Duration::from_millis(50)).await {
                return None;
            }
        }
    }

    fn step_for(&mut self, action: Action) -> Step {
        match action {
            Action::Retry | Action::Reconnect | Action::Pick(_) => FRESH,
            Action::RestartConsole => Step::RestartConsole,
            Action::SwitchConsole => Step::ChooseConsole,
            Action::SignOut => self.sign_out(),
        }
    }

    /// Decides what an error calls for: something the user must settle first,
    /// a fresh sign-in, or the failure screen.
    fn step_after(&mut self, error: Error) -> Step {
        if error.is::<auth::NotSignedIn>() {
            self.account = None;
            return Step::SignIn;
        }
        match error.downcast_ref::<Need>() {
            Some(Need::ConsoleChoice) => return Step::ChooseConsole,
            Some(Need::Console) => return Step::NoConsole,
            None => {}
        }
        // The streaming token expires after some hours; a new sign-in renews it.
        let token_rejected = format!("{error:#}").contains("HTTP 401");
        if token_rejected && !self.renewed_sign_in && self.account.take().is_some() {
            self.renewed_sign_in = true;
            return FRESH;
        }
        Step::Failed(error)
    }

    fn sign_out(&mut self) -> Step {
        self.account = None;
        settings::update(|s| s.console_id = None);
        match store::clear() {
            Ok(()) => Step::SignIn,
            Err(error) => Step::Failed(error.context("could not remove the stored sign-in")),
        }
    }

    async fn sign_in(&mut self) -> Step {
        match self.sign_in_with_code().await {
            Ok(()) => {
                overlay::begin_reconnect();
                FRESH
            }
            Err(error) => Step::Failed(error),
        }
    }

    /// Shows a sign-in code and waits until it has been approved, asking for
    /// a new code whenever one runs out.
    async fn sign_in_with_code(&self) -> Result<()> {
        let http = session::http_client()?;
        loop {
            crate::status!("[auth] asking for a sign-in code");
            let code = unless_closed(&self.closed, auth::begin_login(&http)).await?;
            overlay::show_prompt(sign_in_prompt(&code));
            match unless_closed(&self.closed, auth::finish_login(&http, &code)).await {
                Ok(()) => {
                    crate::status!("[auth] sign-in approved");
                    return Ok(());
                }
                Err(error) if error.is::<auth::CodeExpired>() => continue,
                Err(error) => return Err(error),
            }
        }
    }

    async fn choose_console(&mut self) -> Step {
        overlay::begin_reconnect();
        let consoles = async {
            let http = session::http_client()?;
            let account = session::ensure_signed_in(&http, &self.closed, &mut self.account).await?;
            unless_closed(&self.closed, session::service(&http, account).consoles()).await
        }
        .await;
        let consoles = match consoles {
            Ok(consoles) if consoles.is_empty() => return Step::NoConsole,
            Ok(consoles) => consoles,
            Err(error) => return self.step_after(error),
        };

        let t = text();
        let mut prompt = Prompt::new(Tone::Neutral, t.pick_console_title, t.pick_console_body);
        prompt.list = consoles
            .iter()
            .map(|console| (console.device_name.clone(), console.console_type.clone()))
            .collect();
        prompt.buttons = vec![
            Button::new(t.sign_out, Role::Do(Action::SignOut)).confirmed(),
            Button::new(t.quit, Role::Quit).styled(Style::Danger),
        ];
        overlay::show_prompt(prompt);
        match self.next_action().await {
            Some(Action::Pick(index)) => {
                if let Some(console) = consoles.get(index) {
                    let id = console.server_id.clone();
                    settings::update(|s| s.console_id = Some(id));
                }
                overlay::begin_reconnect();
                FRESH
            }
            Some(action) => self.step_for(action),
            None => Step::Leave,
        }
    }

    async fn no_console(&mut self) -> Step {
        let t = text();
        let mut prompt = Prompt::new(Tone::Neutral, t.no_console_title, t.no_console_body);
        prompt.buttons = vec![
            Button::new(t.try_again, Role::Do(Action::Retry)),
            Button::new(t.sign_out, Role::Do(Action::SignOut)).confirmed(),
            Button::new(t.quit, Role::Quit).styled(Style::Danger),
        ];
        overlay::show_prompt(prompt);
        self.after_prompt().await
    }

    async fn restart_console(&mut self) -> Step {
        overlay::begin_reconnect();
        let console_id = settings::get().console_id;
        match console::restart_and_wait(&self.closed, console_id.as_deref()).await {
            Ok(()) => FRESH,
            Err(error) => Step::Failed(error.context(text().restart_failed)),
        }
    }

    async fn failed(&mut self, error: Error) -> Step {
        let detail = format!("{error:#}");
        eprintln!("[error] {detail}");
        // The console's own settings often explain a failure better than the error does.
        let console_id = settings::get().console_id;
        let lookup = tokio::time::timeout(Duration::from_secs(8), console::explain_failure(console_id.as_deref()));
        let explanation = match unless_closed(&self.closed, async { Ok(lookup.await) }).await {
            Ok(Ok(explanation)) => explanation,
            _ => None,
        };
        overlay::show_prompt(failure_prompt(&detail, explanation));
        self.after_prompt().await
    }

    async fn after_prompt(&mut self) -> Step {
        match self.next_action().await {
            Some(action) => {
                overlay::begin_reconnect();
                self.step_for(action)
            }
            None => Step::Leave,
        }
    }
}

/// A failure is shown as its first, readable part; the whole chain goes into
/// the report the user can copy.
fn headline(message: &str) -> &str {
    if message.chars().count() <= 200 {
        return message;
    }
    message.split_once(": ").map(|(first, _)| first).unwrap_or(message)
}

pub fn failure_prompt(detail: &str, explanation: Option<&str>) -> Prompt {
    let t = text();
    let mut prompt = Prompt::new(Tone::Error, t.failure_title, explanation.unwrap_or(headline(detail)));
    prompt.copy_text = overlay::report(detail);
    prompt.buttons.push(Button::new(t.try_again, Role::Do(Action::Retry)));
    // A console whose link to the Xbox service is stuck is cured by a restart.
    if detail.contains("WNSError") {
        let restart = Button::new(t.restart_console, Role::Do(Action::RestartConsole));
        prompt.buttons.push(restart.styled(Style::Warning).confirmed());
    }
    prompt.buttons.push(Button::new(t.copy_error, Role::Copy));
    prompt.buttons.push(Button::new(t.quit, Role::Quit).styled(Style::Danger));
    prompt
}

pub fn sign_in_prompt(code: &auth::DeviceCode) -> Prompt {
    let t = text();
    let address = code
        .verification_uri
        .trim_start_matches("https://")
        .trim_start_matches("www.");
    let mut prompt = Prompt::new(Tone::Neutral, t.sign_in_title, &fill(t.sign_in_body, address));
    prompt.code = Some(code.user_code.clone());
    // Opening the address with the code attached saves typing it.
    prompt.link = Some(format!("{}?otc={}", code.verification_uri, code.user_code));
    prompt.status = Some(t.sign_in_waiting.to_owned());
    prompt.copy_text = code.user_code.clone();
    prompt.buttons = vec![
        Button::new(t.open_browser, Role::Open),
        Button::new(t.copy_code, Role::Copy),
        Button::new(t.quit, Role::Quit).styled(Style::Danger),
    ];
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_failures_show_only_their_first_part() {
        assert_eq!(headline("short: detail"), "short: detail");
        let long = format!("The console does not answer: {}", "x".repeat(300));
        assert_eq!(headline(&long), "The console does not answer");
        let no_separator = "y".repeat(300);
        assert_eq!(headline(&no_separator), no_separator);
    }

    #[test]
    fn a_restart_is_offered_only_when_it_would_help() {
        let has_restart = |prompt: &Prompt| {
            prompt.buttons.iter().any(|b| b.role == Role::Do(Action::RestartConsole) && b.confirm)
        };
        assert!(has_restart(&failure_prompt("console refused the session: WNSError", None)));
        assert!(!has_restart(&failure_prompt("timed out", None)));
        let explained = failure_prompt("technical chain", Some("Turn the console on."));
        assert_eq!(explained.body, "Turn the console on.");
        assert!(explained.copy_text.contains("technical chain"));
    }

    #[test]
    fn the_sign_in_prompt_shows_a_short_address_and_links_with_the_code() {
        let code = auth::DeviceCode {
            user_code: "ABCD1234".into(),
            device_code: String::new(),
            verification_uri: "https://www.microsoft.com/link".into(),
            expires_in: 900,
            interval: 5,
        };
        let prompt = sign_in_prompt(&code);
        assert!(prompt.body.contains("microsoft.com/link") && !prompt.body.contains("https://"));
        assert_eq!(prompt.link.as_deref(), Some("https://www.microsoft.com/link?otc=ABCD1234"));
        assert_eq!(prompt.copy_text, "ABCD1234");
    }
}
