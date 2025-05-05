use std::fs;
use std::path::PathBuf;

use crate::patcher::{get_patcher_name, PatcherCommand, PatcherConfiguration};
use crate::process::start_executable;
use serde::Deserialize;
use serde_json::Value;
use tinyfiledialogs as tfd;
use web_view::{Content, Handle, WebView};

/// 'Opaque" struct that can be used to update the UI.
pub struct UiController {
    web_view_handle: Handle<WebViewUserData>,
}
impl UiController {
    pub fn new(web_view: &WebView<'_, WebViewUserData>) -> UiController {
        UiController {
            web_view_handle: web_view.handle(),
        }
    }

    /// Allows another thread to indicate the current status of the patching process.
    ///
    /// This updates the UI with useful information.
    pub fn dispatch_patching_status(&self, status: PatchingStatus) -> Result<(), web_view::Error> {
        self.web_view_handle.dispatch(move |webview| {
            let result = match status {
                PatchingStatus::Ready => {
                    let js_code = r#"
                        const progressBar = document.getElementById('download-progress-bar');
                        const progressText = document.getElementById('download-progress-text');
                        const playButton = document.getElementById('button-play');
                        
                        progressBar.style.width = '100%';
                        progressBar.setAttribute('aria-valuenow', '100');
                        progressBar.classList.remove('bg-warning', 'bg-danger');
                        progressBar.classList.add('bg-primary');
                        progressText.textContent = 'Ready';
                        playButton.disabled = false;
                    "#;
                    if let Err(e) = webview.eval(js_code) {
                        log::warn!("Failed to set ready status: {}.", e);
                    }
                    Ok(())
                },
                PatchingStatus::Error(msg) => {
                    webview.eval(&format!("patchingStatusError(\"{}\")", msg))
                }
                PatchingStatus::DownloadInProgress(nb_downloaded, nb_total, bytes_per_sec) => {
                    webview.eval(&format!(
                        "patchingStatusDownloading({}, {}, {})",
                        nb_downloaded, nb_total, bytes_per_sec
                    ))
                }
                PatchingStatus::InstallationInProgress(nb_installed, nb_total) => webview.eval(
                    &format!("patchingStatusInstalling({}, {})", nb_installed, nb_total),
                ),
                PatchingStatus::ManualPatchApplied(name) => {
                    webview.eval(&format!("patchingStatusPatchApplied(\"{}\")", name))
                }
            };
            if let Err(e) = result {
                log::warn!("Failed to dispatch patching status: {}.", e);
            }
            Ok(())
        })
    }

    pub fn set_patch_in_progress(&self, value: bool) {
        if let Err(e) = self.web_view_handle.dispatch(move |webview| {
            webview.user_data_mut().patching_in_progress = value;
            Ok(())
        }) {
            log::warn!("Failed to dispatch patching status: {}.", e);
        }
    }
}

/// Used to indicate the current status of the patching process.
pub enum PatchingStatus {
    Ready,
    Error(String),                         // Error message
    DownloadInProgress(usize, usize, u64), // Downloaded files, Total number, Bytes per second
    InstallationInProgress(usize, usize),  // Installed patches, Total number
    ManualPatchApplied(String),            // Patch file name
}

pub struct WebViewUserData {
    patcher_config: PatcherConfiguration,
    patching_thread_tx: flume::Sender<PatcherCommand>,
    patching_in_progress: bool,
}
impl WebViewUserData {
    pub fn new(
        patcher_config: PatcherConfiguration,
        patching_thread_tx: flume::Sender<PatcherCommand>,
    ) -> WebViewUserData {
        WebViewUserData {
            patcher_config,
            patching_thread_tx,
            patching_in_progress: false,
        }
    }
}
impl Drop for WebViewUserData {
    fn drop(&mut self) {
        // Ask the patching thread to stop whenever WebViewUserData is dropped
        let _res = self.patching_thread_tx.try_send(PatcherCommand::Quit);
    }
}

/// Creates a `WebView` object with the appropriate settings for our needs.
pub fn build_webview<'a>(
    title: &'a str,
    user_data: WebViewUserData,
) -> web_view::WVResult<WebView<'a, WebViewUserData>> {
    web_view::builder()
        .title(title)
        .content(Content::Url(user_data.patcher_config.web.index_url.clone()))
        .size(
            user_data.patcher_config.window.width,
            user_data.patcher_config.window.height,
        )
        .resizable(user_data.patcher_config.window.resizable)
        .user_data(user_data)
        .invoke_handler(|webview, arg| {
            match arg {
                "play" => handle_play(webview),
                "setup" => handle_setup(webview),
                "exit" => handle_exit(webview),
                "start_update" => handle_start_update(webview),
                "cancel_update" => handle_cancel_update(webview),
                "reset_cache" => handle_reset_cache(webview),
                "manual_patch" => handle_manual_patch(webview),
                request => handle_json_request(webview, request),
            }
            Ok(())
        })
        .build()
}

/// Opens the configured game client with the configured arguments.
///
/// This function can create elevated processes on Windows with UAC activated.
fn handle_play(webview: &mut WebView<WebViewUserData>) {
    let client_arguments = webview.user_data().patcher_config.play.arguments.clone();
    start_game_client(webview, &client_arguments);
}

/// Opens the configured 'Setup' software with the configured arguments.
///
/// This function can create elevated processes on Windows with UAC activated.
fn handle_setup(webview: &mut WebView<WebViewUserData>) {
    let setup_exe: &String = &webview.user_data().patcher_config.setup.path;
    let setup_arguments = &webview.user_data().patcher_config.setup.arguments;
    let exit_on_success = webview
        .user_data()
        .patcher_config
        .setup
        .exit_on_success
        .unwrap_or(false);
    match start_executable(setup_exe, setup_arguments) {
        Ok(success) => {
            if success {
                log::trace!("Setup software started");
                if exit_on_success {
                    webview.exit();
                }
            }
        }
        Err(e) => {
            log::warn!("Failed to start setup software: {}", e);
        }
    }
}

/// Exits the patcher cleanly.
fn handle_exit(webview: &mut WebView<WebViewUserData>) {
    webview.exit();
}

/// Starts the patching task/thread.
fn handle_start_update(webview: &mut WebView<WebViewUserData>) {
    // Patching is already in progress, abort.
    if webview.user_data().patching_in_progress {
        let res = webview.eval("notificationInProgress()");
        if let Err(e) = res {
            log::warn!("Failed to dispatch notification: {}.", e);
        }
        return;
    }

    let send_res = webview
        .user_data_mut()
        .patching_thread_tx
        .send(PatcherCommand::StartUpdate);
    if send_res.is_ok() {
        log::trace!("Sent StartUpdate command to patching thread");
    }
}

/// Cancels the patching task/thread.
fn handle_cancel_update(webview: &mut WebView<WebViewUserData>) {
    if webview
        .user_data_mut()
        .patching_thread_tx
        .send(PatcherCommand::CancelUpdate)
        .is_ok()
    {
        log::trace!("Sent CancelUpdate command to patching thread");
    }
}

/// Resets the patcher cache (which is used to keep track of already applied
/// patches).
fn handle_reset_cache(_webview: &mut WebView<WebViewUserData>) {
    if let Ok(patcher_name) = get_patcher_name() {
        let cache_file_path = PathBuf::from(patcher_name).with_extension("dat");
        if let Err(e) = fs::remove_file(cache_file_path) {
            log::warn!("Failed to remove the cache file: {}", e);
        }
    }
}

/// Asks the user to provide a patch file to apply
fn handle_manual_patch(webview: &mut WebView<WebViewUserData>) {
    // Patching is already in progress, abort.
    if webview.user_data().patching_in_progress {
        let res = webview.eval("notificationInProgress()");
        if let Err(e) = res {
            log::warn!("Failed to dispatch notification: {}.", e);
        }
        return;
    }

    let opt_path = tfd::open_file_dialog(
        "Select a file",
        "",
        Some((&["*.thor"], "Patch Files (*.thor)")),
    );
    if let Some(path) = opt_path {
        log::info!("Requesting manual patch '{}'", path);
        if webview
            .user_data_mut()
            .patching_thread_tx
            .send(PatcherCommand::ApplyPatch(PathBuf::from(path)))
            .is_ok()
        {
            log::trace!("Sent ApplyPatch command to patching thread");
        }
    }
}

/// Parses JSON requests (for invoking functions with parameters) and dispatches
/// them to the invoked function.
fn handle_json_request(webview: &mut WebView<WebViewUserData>, request: &str) {
    let result: serde_json::Result<Value> = serde_json::from_str(request);
    match result {
        Err(e) => {
            log::error!("Invalid JSON request: {}", e);
        }
        Ok(json_req) => {
            let function_name = json_req["function"].as_str();
            if let Some(function_name) = function_name {
                let function_params = json_req["parameters"].clone();
                match function_name {
                    "login" => handle_login(webview, function_params),
                    "open_url" => handle_open_url(function_params),
                    _ => {
                        log::error!("Unknown function '{}'", function_name);
                    }
                }
            }
        }
    }
}

/// Parameters expected for the login function
#[derive(Deserialize)]
struct LoginParameters {
    login: String,
    password: String,
}

/// Launches the game client with the given credentials
fn handle_login(webview: &mut WebView<WebViewUserData>, parameters: Value) {
    let result: serde_json::Result<LoginParameters> = serde_json::from_value(parameters);
    match result {
        Err(e) => log::error!("Invalid arguments given for 'login': {}", e),
        Ok(login_params) => {
            // Push credentials to the list of arguments first
            let mut play_arguments: Vec<String> = vec![
                format!("-t:{}", login_params.password),
                login_params.login,
                "server".to_string(),
            ];
            play_arguments.extend(
                webview
                    .user_data()
                    .patcher_config
                    .play
                    .arguments
                    .iter()
                    .cloned(),
            );
            start_game_client(webview, &play_arguments);
        }
    }
}

/// Parameters expected for the open_url function
#[derive(Deserialize)]
struct OpenUrlParameters {
    url: String,
}

/// Opens an URL with the native URL Handler
fn handle_open_url(parameters: Value) {
    let result: serde_json::Result<OpenUrlParameters> = serde_json::from_value(parameters);
    match result {
        Err(e) => log::error!("Invalid arguments given for 'open_url': {}", e),
        Ok(params) => match open::that(params.url) {
            Ok(exit_status) => {
                if !exit_status.success() {
                    if let Some(code) = exit_status.code() {
                        log::error!("Command returned non-zero exit status {}!", code);
                    }
                }
            }
            Err(why) => {
                log::error!("Error open_url function: '{}'", why);
            }
        },
    }
}

fn start_game_client(webview: &mut WebView<WebViewUserData>, client_arguments: &[String]) {
    let client_exe: &String = &webview.user_data().patcher_config.play.path;
    let exit_on_success = webview
        .user_data()
        .patcher_config
        .play
        .exit_on_success
        .unwrap_or(true);
    match start_executable(client_exe, client_arguments) {
        Ok(success) => {
            if success {
                log::trace!("Client started");
                if exit_on_success {
                    webview.exit();
                }
            }
        }
        Err(e) => {
            log::warn!("Failed to start client: {}", e);
        }
    }
}
