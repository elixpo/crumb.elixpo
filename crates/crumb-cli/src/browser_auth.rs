use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crumb_auth::SecretString;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

const CALLBACK_ADDRESS: &str = "127.0.0.1:3000";
const CALLBACK_URL: &str = "http://localhost:3000/auth/connector/callback";
const DEFAULT_ACCOUNT_ORIGIN: &str = "https://crumb.elixpo.com";
const FLOW_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Serialize)]
struct ExchangeRequest<'a> {
    code: &'a str,
    code_verifier: &'a str,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    access_token: String,
}

/// Connects an Elixpo account through the browser and returns its provider credential.
///
/// # Errors
///
/// Returns an error when the loopback listener, browser handoff, token exchange,
/// or account-service validation fails.
pub(crate) fn connect(writer: &mut dyn Write) -> Result<SecretString> {
    let listener = TcpListener::bind(CALLBACK_ADDRESS)
        .context("could not listen on localhost:3000 for the account callback")?;
    listener.set_nonblocking(true)?;

    let state = random_token(32)?;
    let verifier = random_token(48)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let origin = trusted_account_origin()?;
    let mut connect_url = origin.join("/connect")?;
    connect_url
        .query_pairs_mut()
        .append_pair("redirect_uri", CALLBACK_URL)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge);

    writeln!(writer, "Connect your account in the browser:")?;
    writeln!(writer, "{connect_url}")?;
    if !open_browser(connect_url.as_str()) {
        writeln!(writer, "Open the URL above manually to continue.")?;
    }
    writeln!(writer, "Waiting for the secure callback (up to 5 minutes)…")?;
    writer.flush()?;

    let code = wait_for_callback(&listener, &state)?;
    let endpoint = origin.join("/api/terminal/exchange")?;
    let response = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?
        .post(endpoint)
        .json(&ExchangeRequest {
            code: &code,
            code_verifier: &verifier,
        })
        .send()
        .context("account token exchange failed")?;
    if !response.status().is_success() {
        bail!("account connection was rejected; run `crumb auth login` again");
    }
    let exchange = response
        .json::<ExchangeResponse>()
        .context("account service returned an invalid response")?;
    if exchange.access_token.trim().is_empty() {
        bail!("account service returned an empty credential");
    }
    Ok(SecretString::new(exchange.access_token))
}

fn trusted_account_origin() -> Result<Url> {
    let value =
        std::env::var("CRUMB_ACCOUNT_URL").unwrap_or_else(|_| DEFAULT_ACCOUNT_ORIGIN.to_owned());
    validate_account_origin(&value)
}

fn validate_account_origin(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("CRUMB_ACCOUNT_URL is invalid")?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    if url.scheme() != "https" && !(local && url.scheme() == "http") {
        bail!("CRUMB_ACCOUNT_URL must use HTTPS unless it is localhost");
    }
    Ok(url)
}

fn random_token(length: usize) -> Result<String> {
    let mut bytes = vec![0_u8; length];
    getrandom::fill(&mut bytes).context("secure random generation failed")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn wait_for_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    let deadline = Instant::now() + FLOW_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => return handle_callback(&mut stream, expected_state),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("account connection timed out");
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn handle_callback(stream: &mut TcpStream, expected_state: &str) -> Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buffer = [0_u8; 8_192];
    let length = stream.read(&mut buffer)?;
    let request = std::str::from_utf8(&buffer[..length]).context("callback was not valid HTTP")?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("callback request was malformed"))?;
    let url = Url::parse(&format!("http://localhost{target}"))?;
    let values = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    let result = if url.path() != "/auth/connector/callback"
        || values
            .get("state")
            .is_none_or(|state| state != expected_state)
    {
        Err(anyhow!("account callback state did not match"))
    } else if let Some(error) = values.get("error") {
        Err(anyhow!("account connection failed: {error}"))
    } else {
        values
            .get("code")
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("account callback did not include a code"))
    };
    let (status, heading) = if result.is_ok() {
        ("200 OK", "Crumb is connected")
    } else {
        ("400 Bad Request", "Crumb could not connect")
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{heading}</title><style>body{{font:16px system-ui;display:grid;place-items:center;min-height:90vh;background:#f7f5f2;color:#111}}main{{background:white;border:1px solid #ddd;border-radius:20px;padding:40px;max-width:440px}}h1{{letter-spacing:-.04em}}</style><main><h1>{heading}</h1><p>You can close this page and return to the terminal.</p></main>"
    );
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    result
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "linux")]
    let result = Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "browser launch is unsupported",
    ));
    result.is_ok()
}

#[cfg(test)]
mod tests {
    use super::validate_account_origin;

    #[test]
    fn production_account_origin_uses_https() {
        let origin = validate_account_origin("https://crumb.elixpo.com")
            .expect("production origin should be trusted");
        assert_eq!(origin.scheme(), "https");
    }

    #[test]
    fn remote_http_account_origin_is_rejected() {
        assert!(validate_account_origin("http://crumb.example").is_err());
        assert!(validate_account_origin("http://localhost:3001").is_ok());
    }
}
