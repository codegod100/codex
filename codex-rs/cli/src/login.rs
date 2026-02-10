use codex_core::CodexAuth;
use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::auth::AuthMode;
use codex_core::auth::CLIENT_ID;
use codex_core::auth::load_copilot_access_token;
use codex_core::auth::login_with_api_key;
use codex_core::auth::login_with_copilot_access_token;
use codex_core::auth::logout;
use codex_core::auth::logout_copilot;
use codex_core::config::Config;
use codex_login::ServerOptions;
use codex_login::run_device_code_login;
use codex_login::run_login_server;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_utils_cli::CliConfigOverrides;
use serde::Deserialize;
use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

const CHATGPT_LOGIN_DISABLED_MESSAGE: &str =
    "ChatGPT login is disabled. Use API key login instead.";
const API_KEY_LOGIN_DISABLED_MESSAGE: &str =
    "API key login is disabled. Use ChatGPT login instead.";
const LOGIN_SUCCESS_MESSAGE: &str = "Successfully logged in";
const DEFAULT_GITHUB_DEVICE_CODE_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_DEVICE_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_DEVICE_VERIFICATION_URL: &str = "https://github.com/login/device";
const GITHUB_DEVICE_SCOPE: &str = "read:user";

#[derive(Debug, Deserialize)]
struct GitHubDeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GitHubDeviceTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

fn print_login_server_start(actual_port: u16, auth_url: &str) {
    eprintln!(
        "Starting local login server on http://localhost:{actual_port}.\nIf your browser did not open, navigate to this URL to authenticate:\n\n{auth_url}"
    );
}

pub async fn login_with_chatgpt(
    codex_home: PathBuf,
    forced_chatgpt_workspace_id: Option<String>,
    cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<()> {
    let opts = ServerOptions::new(
        codex_home,
        CLIENT_ID.to_string(),
        forced_chatgpt_workspace_id,
        cli_auth_credentials_store_mode,
    );
    let server = run_login_server(opts)?;

    print_login_server_start(server.actual_port, &server.auth_url);

    server.block_until_done().await
}

pub async fn run_login_with_chatgpt(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;

    if matches!(config.forced_login_method, Some(ForcedLoginMethod::Api)) {
        eprintln!("{CHATGPT_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }

    let forced_chatgpt_workspace_id = config.forced_chatgpt_workspace_id.clone();

    match login_with_chatgpt(
        config.codex_home,
        forced_chatgpt_workspace_id,
        config.cli_auth_credentials_store_mode,
    )
    .await
    {
        Ok(_) => {
            eprintln!("{LOGIN_SUCCESS_MESSAGE}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error logging in: {e}");
            std::process::exit(1);
        }
    }
}

pub async fn run_login_with_api_key(
    cli_config_overrides: CliConfigOverrides,
    api_key: String,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;

    if matches!(config.forced_login_method, Some(ForcedLoginMethod::Chatgpt)) {
        eprintln!("{API_KEY_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }

    match login_with_api_key(
        &config.codex_home,
        &api_key,
        config.cli_auth_credentials_store_mode,
    ) {
        Ok(_) => {
            eprintln!("{LOGIN_SUCCESS_MESSAGE}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error logging in: {e}");
            std::process::exit(1);
        }
    }
}

pub fn read_api_key_from_stdin() -> String {
    let mut stdin = std::io::stdin();

    if stdin.is_terminal() {
        eprintln!(
            "--with-api-key expects the API key on stdin. Try piping it, e.g. `printenv OPENAI_API_KEY | codex login --with-api-key`."
        );
        std::process::exit(1);
    }

    eprintln!("Reading API key from stdin...");

    let mut buffer = String::new();
    if let Err(err) = stdin.read_to_string(&mut buffer) {
        eprintln!("Failed to read API key from stdin: {err}");
        std::process::exit(1);
    }

    let api_key = buffer.trim().to_string();
    if api_key.is_empty() {
        eprintln!("No API key provided via stdin.");
        std::process::exit(1);
    }

    api_key
}

/// Login using the OAuth device code flow.
pub async fn run_login_with_device_code(
    cli_config_overrides: CliConfigOverrides,
    issuer_base_url: Option<String>,
    client_id: Option<String>,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    if matches!(config.forced_login_method, Some(ForcedLoginMethod::Api)) {
        eprintln!("{CHATGPT_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }
    let forced_chatgpt_workspace_id = config.forced_chatgpt_workspace_id.clone();
    let mut opts = ServerOptions::new(
        config.codex_home,
        client_id.unwrap_or(CLIENT_ID.to_string()),
        forced_chatgpt_workspace_id,
        config.cli_auth_credentials_store_mode,
    );
    if let Some(iss) = issuer_base_url {
        opts.issuer = iss;
    }
    match run_device_code_login(opts).await {
        Ok(()) => {
            eprintln!("{LOGIN_SUCCESS_MESSAGE}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error logging in with device code: {e}");
            std::process::exit(1);
        }
    }
}

/// Prefers device-code login (with `open_browser = false`) when headless environment is detected, but keeps
/// `codex login` working in environments where device-code may be disabled/feature-gated.
/// If `run_device_code_login` returns `ErrorKind::NotFound` ("device-code unsupported"), this
/// falls back to starting the local browser login server.
pub async fn run_login_with_device_code_fallback_to_browser(
    cli_config_overrides: CliConfigOverrides,
    issuer_base_url: Option<String>,
    client_id: Option<String>,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    if matches!(config.forced_login_method, Some(ForcedLoginMethod::Api)) {
        eprintln!("{CHATGPT_LOGIN_DISABLED_MESSAGE}");
        std::process::exit(1);
    }

    let forced_chatgpt_workspace_id = config.forced_chatgpt_workspace_id.clone();
    let mut opts = ServerOptions::new(
        config.codex_home,
        client_id.unwrap_or(CLIENT_ID.to_string()),
        forced_chatgpt_workspace_id,
        config.cli_auth_credentials_store_mode,
    );
    if let Some(iss) = issuer_base_url {
        opts.issuer = iss;
    }
    opts.open_browser = false;

    match run_device_code_login(opts.clone()).await {
        Ok(()) => {
            eprintln!("{LOGIN_SUCCESS_MESSAGE}");
            std::process::exit(0);
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("Device code login is not enabled; falling back to browser login.");
                match run_login_server(opts) {
                    Ok(server) => {
                        print_login_server_start(server.actual_port, &server.auth_url);
                        match server.block_until_done().await {
                            Ok(()) => {
                                eprintln!("{LOGIN_SUCCESS_MESSAGE}");
                                std::process::exit(0);
                            }
                            Err(e) => {
                                eprintln!("Error logging in: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Error logging in: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                eprintln!("Error logging in with device code: {e}");
                std::process::exit(1);
            }
        }
    }
}

pub async fn run_login_with_copilot(
    cli_config_overrides: CliConfigOverrides,
    client_id: Option<String>,
) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;
    let client_id = client_id.unwrap_or_else(|| DEFAULT_GITHUB_DEVICE_CODE_CLIENT_ID.to_string());
    let client = reqwest::Client::new();

    let device_code_response = match client
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id.as_str()),
            ("scope", GITHUB_DEVICE_SCOPE),
        ])
        .send()
        .await
    {
        Ok(response) => {
            if !response.status().is_success() {
                eprintln!(
                    "Copilot device code request failed with status {}",
                    response.status()
                );
                std::process::exit(1);
            }
            match response.json::<GitHubDeviceCodeResponse>().await {
                Ok(parsed) => parsed,
                Err(err) => {
                    eprintln!("Failed to parse Copilot device code response: {err}");
                    std::process::exit(1);
                }
            }
        }
        Err(err) => {
            eprintln!("Failed to request Copilot device code: {err}");
            std::process::exit(1);
        }
    };

    let verification_uri = device_code_response
        .verification_uri
        .as_deref()
        .unwrap_or(GITHUB_DEVICE_VERIFICATION_URL);
    let interval = device_code_response.interval.unwrap_or(5);

    eprintln!(
        "To sign in to GitHub Copilot:\n\n1. Open: {verification_uri}\n2. Enter code: {}\n",
        device_code_response.user_code
    );
    eprintln!("Waiting for authorization...");

    let started = tokio::time::Instant::now();
    let max_wait = Duration::from_secs(device_code_response.expires_in.max(60));

    loop {
        if started.elapsed() >= max_wait {
            eprintln!("Copilot device authorization timed out.");
            std::process::exit(1);
        }

        let token_response = match client
            .post(GITHUB_DEVICE_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id.as_str()),
                ("device_code", device_code_response.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
        {
            Ok(response) => {
                if !response.status().is_success() {
                    eprintln!(
                        "Copilot token exchange failed with status {}",
                        response.status()
                    );
                    std::process::exit(1);
                }
                match response.json::<GitHubDeviceTokenResponse>().await {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        eprintln!("Failed to parse Copilot token response: {err}");
                        std::process::exit(1);
                    }
                }
            }
            Err(err) => {
                eprintln!("Failed to poll Copilot token endpoint: {err}");
                std::process::exit(1);
            }
        };

        if let Some(access_token) = token_response.access_token {
            match login_with_copilot_access_token(&config.codex_home, &access_token) {
                Ok(()) => {
                    eprintln!("{LOGIN_SUCCESS_MESSAGE}");
                    std::process::exit(0);
                }
                Err(err) => {
                    eprintln!("Error saving Copilot token: {err}");
                    std::process::exit(1);
                }
            }
        }

        match token_response.error.as_deref() {
            Some("authorization_pending") => {
                tokio::time::sleep(Duration::from_secs(interval)).await
            }
            Some("slow_down") => tokio::time::sleep(Duration::from_secs(interval + 5)).await,
            Some("expired_token") => {
                eprintln!("Copilot device code expired. Run `codex login --copilot` again.");
                std::process::exit(1);
            }
            Some("access_denied") => {
                eprintln!("Copilot authorization was denied.");
                std::process::exit(1);
            }
            Some(other) => {
                let detail = token_response
                    .error_description
                    .as_deref()
                    .unwrap_or("no details");
                eprintln!("Copilot authorization failed: {other} ({detail})");
                std::process::exit(1);
            }
            None => {
                eprintln!("Copilot token response did not include an access token.");
                std::process::exit(1);
            }
        }
    }
}

pub async fn run_login_status(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;

    let mut logged_in = false;

    match CodexAuth::from_auth_storage(&config.codex_home, config.cli_auth_credentials_store_mode) {
        Ok(Some(auth)) => {
            logged_in = true;
            match auth.auth_mode() {
                AuthMode::ApiKey => match auth.get_token() {
                    Ok(api_key) => {
                        eprintln!(
                            "Logged in using an OpenAI API key - {}",
                            safe_format_key(&api_key)
                        );
                    }
                    Err(e) => {
                        eprintln!("Unexpected error retrieving OpenAI API key: {e}");
                        std::process::exit(1);
                    }
                },
                AuthMode::Chatgpt => {
                    eprintln!("Logged in using ChatGPT");
                }
            }
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("Error checking OpenAI login status: {e}");
            std::process::exit(1);
        }
    }

    match load_copilot_access_token(&config.codex_home) {
        Ok(Some(_)) => {
            logged_in = true;
            eprintln!("Logged in to GitHub Copilot");
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("Error checking Copilot login status: {e}");
            std::process::exit(1);
        }
    }

    if logged_in {
        std::process::exit(0);
    }

    eprintln!("Not logged in");
    std::process::exit(1);
}

pub async fn run_logout(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;

    let removed_openai = match logout(&config.codex_home, config.cli_auth_credentials_store_mode) {
        Ok(removed) => removed,
        Err(e) => {
            eprintln!("Error logging out OpenAI credentials: {e}");
            std::process::exit(1);
        }
    };

    let removed_copilot = match logout_copilot(&config.codex_home) {
        Ok(removed) => removed,
        Err(e) => {
            eprintln!("Error logging out Copilot credentials: {e}");
            std::process::exit(1);
        }
    };

    if removed_openai || removed_copilot {
        eprintln!("Successfully logged out");
    } else {
        eprintln!("Not logged in");
    }
    std::process::exit(0);
}

async fn load_config_or_exit(cli_config_overrides: CliConfigOverrides) -> Config {
    let cli_overrides = match cli_config_overrides.parse_overrides() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing -c overrides: {e}");
            std::process::exit(1);
        }
    };

    match Config::load_with_cli_overrides(cli_overrides).await {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Error loading configuration: {e}");
            std::process::exit(1);
        }
    }
}

fn safe_format_key(key: &str) -> String {
    if key.len() <= 13 {
        return "***".to_string();
    }
    let prefix = &key[..8];
    let suffix = &key[key.len() - 5..];
    format!("{prefix}***{suffix}")
}

#[cfg(test)]
mod tests {
    use super::safe_format_key;

    #[test]
    fn formats_long_key() {
        let key = "sk-proj-1234567890ABCDE";
        assert_eq!(safe_format_key(key), "sk-proj-***ABCDE");
    }

    #[test]
    fn short_key_returns_stars() {
        let key = "sk-proj-12345";
        assert_eq!(safe_format_key(key), "***");
    }
}
