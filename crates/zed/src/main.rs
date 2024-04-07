mod zed;

use anyhow::{anyhow, Context as _, Result};
use backtrace::Backtrace;
use chrono::Utc;
use clap::{command, Parser};
use cli::FORCE_CLI_MODE_ENV_VAR_NAME;
use db::kvp::KEY_VALUE_STORE;
use editor::Editor;
use env_logger::Builder;
use fs::RealFs;
use futures::StreamExt;
use gpui::{App, AppContext, AsyncAppContext, Context, SemanticVersion, Task};
use image_viewer;
use language::LanguageRegistry;
use log::LevelFilter;

use assets::Assets;
use mimalloc::MiMalloc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use settings::{default_settings, handle_settings_file_changes, watch_config_file, SettingsStore};
use simplelog::ConfigBuilder;
use smol::process::Command;
use std::{
    env,
    fs::OpenOptions,
    io::{IsTerminal, Write},
    panic,
    path::Path,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    thread,
};
use theme::{ActiveTheme, SystemAppearance, ThemeRegistry, ThemeSettings};
use util::{maybe, paths, ResultExt};
use welcome::{show_welcome_view, FIRST_OPEN};
use workspace::{AppState, WorkspaceStore};
use zed::{
    app_menus, build_window_options, ensure_only_instance, handle_cli_connection,
    handle_keymap_file_changes, initialize_workspace, open_paths_with_positions, IsOnlyInstance,
    OpenListener, OpenRequest,
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    menu::init();
    zed_actions::init();

    init_paths();
    init_logger();

    if ensure_only_instance() != IsOnlyInstance::Yes {
        return;
    }

    log::info!("========== starting zed ==========");
    let app = App::new().with_assets(Assets);

    init_panic_hook(&app);

    let fs = Arc::new(RealFs);
    let user_settings_file_rx = watch_config_file(
        &app.background_executor(),
        fs.clone(),
        paths::SETTINGS.clone(),
    );
    let user_keymap_file_rx = watch_config_file(
        &app.background_executor(),
        fs.clone(),
        paths::KEYMAP.clone(),
    );

    let login_shell_env_loaded = if stdout_is_a_pty() {
        Task::ready(())
    } else {
        app.background_executor().spawn(async {
            load_login_shell_environment().await.log_err();
        })
    };

    let (listener, mut open_rx) = OpenListener::new();
    let listener = Arc::new(listener);
    let open_listener = listener.clone();
    app.on_open_urls(move |urls| open_listener.open_urls(urls));
    app.on_reopen(move |cx| {
        if let Some(app_state) = AppState::try_global(cx).and_then(|app_state| app_state.upgrade())
        {
            workspace::open_new(app_state, cx, |workspace, cx| {
                Editor::new_file(workspace, &Default::default(), cx)
            })
            .detach();
        }
    });

    app.run(move |cx| {
        SystemAppearance::init(cx);
        OpenListener::set_global(listener.clone(), cx);

        load_embedded_fonts(cx);

        let mut store = SettingsStore::default();
        store
            .set_default_settings(default_settings().as_ref(), cx)
            .unwrap();
        cx.set_global(store);
        handle_settings_file_changes(user_settings_file_rx, cx);
        handle_keymap_file_changes(user_keymap_file_rx, cx);

        let mut languages =
            LanguageRegistry::new(login_shell_env_loaded, cx.background_executor().clone());
        languages.set_language_server_download_dir(paths::LANGUAGES_DIR.clone());
        let languages = Arc::new(languages);

        language::init(cx);
        languages::init(languages.clone(), cx);
        let workspace_store = cx.new_model(|_| WorkspaceStore::new());

        zed::init(cx);
        theme::init(theme::LoadThemes::All(Box::new(Assets)), cx);
        project::Project::init(cx);
        command_palette::init(cx);
        language::init(cx);
        editor::init(cx);
        image_viewer::init(cx);
        diagnostics::init(cx);
        load_user_themes_in_background(fs.clone(), cx);
        watch_themes(fs.clone(), cx);

        watch_file_types(fs.clone(), cx);

        languages.set_theme(cx.theme().clone());
        cx.observe_global::<SettingsStore>({
            let languages = languages.clone();

            move |cx| {
                languages.set_theme(cx.theme().clone());
            }
        })
        .detach();

        let app_state = Arc::new(AppState {
            languages: languages.clone(),
            fs: fs.clone(),
            build_window_options,
            workspace_store,
        });
        AppState::set_global(Arc::downgrade(&app_state), cx);

        workspace::init(app_state.clone(), cx);
        recent_projects::init(cx);

        go_to_line::init(cx);
        file_finder::init(cx);
        outline::init(cx);
        project_symbols::init(cx);
        project_panel::init(Assets, cx);
        tasks_ui::init(cx);
        search::init(cx);
        vim::init(cx);
        terminal_view::init(cx);

        journal::init(app_state.clone(), cx);
        language_selector::init(cx);
        theme_selector::init(cx);
        language_tools::init(cx);
        markdown_preview::init(cx);
        welcome::init(cx);

        cx.set_menus(app_menus());
        initialize_workspace(app_state.clone(), cx);

        cx.activate(true);

        let args = Args::parse();

        let urls: Vec<_> = args
            .paths_or_urls
            .iter()
            .filter_map(|arg| parse_url_arg(arg).log_err())
            .collect();

        if !urls.is_empty() {
            listener.open_urls(urls)
        }

        match open_rx
            .try_next()
            .ok()
            .flatten()
            .and_then(|urls| OpenRequest::parse(urls).log_err())
        {
            Some(request) => {
                handle_open_request(request, app_state.clone(), cx);
            }
            None => cx
                .spawn({
                    let app_state = app_state.clone();
                    |cx| async move { restore_or_create_workspace(app_state, cx).await }
                })
                .detach(),
        }

        let app_state = app_state.clone();
        cx.spawn(move |cx| async move {
            while let Some(urls) = open_rx.next().await {
                cx.update(|cx| {
                    if let Some(request) = OpenRequest::parse(urls).log_err() {
                        handle_open_request(request, app_state.clone(), cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    });
}

fn handle_open_request(
    request: OpenRequest,
    app_state: Arc<AppState>,
    cx: &mut AppContext,
) -> bool {
    if let Some(connection) = request.cli_connection {
        let app_state = app_state.clone();
        cx.spawn(move |cx| handle_cli_connection(connection, app_state, cx))
            .detach();
        return false;
    }

    let mut task = None;
    if !request.open_paths.is_empty() {
        let app_state = app_state.clone();
        task = Some(cx.spawn(|mut cx| async move {
            let (_window, results) = open_paths_with_positions(
                &request.open_paths,
                app_state,
                workspace::OpenOptions::default(),
                &mut cx,
            )
            .await?;
            for result in results.into_iter().flatten() {
                if let Err(err) = result {
                    log::error!("Error opening path: {err}",);
                }
            }
            anyhow::Ok(())
        }));
    }

    if let Some(task) = task {
        task.detach_and_log_err(cx)
    }
    false
}

async fn restore_or_create_workspace(app_state: Arc<AppState>, cx: AsyncAppContext) {
    maybe!(async {
        if let Some(location) = workspace::last_opened_workspace_paths().await {
            cx.update(|cx| {
                workspace::open_paths(
                    location.paths().as_ref(),
                    app_state,
                    workspace::OpenOptions::default(),
                    cx,
                )
            })?
            .await
            .log_err();
        } else if matches!(KEY_VALUE_STORE.read_kvp(FIRST_OPEN), Ok(None)) {
            cx.update(|cx| show_welcome_view(app_state, cx)).log_err();
        } else {
            cx.update(|cx| {
                workspace::open_new(app_state, cx, |workspace, cx| {
                    Editor::new_file(workspace, &Default::default(), cx)
                })
                .detach();
            })?;
        }
        anyhow::Ok(())
    })
    .await
    .log_err();
}

fn init_paths() {
    std::fs::create_dir_all(&*util::paths::CONFIG_DIR).expect("could not create config path");
    std::fs::create_dir_all(&*util::paths::EXTENSIONS_DIR)
        .expect("could not create extensions path");
    std::fs::create_dir_all(&*util::paths::LANGUAGES_DIR).expect("could not create languages path");
    std::fs::create_dir_all(&*util::paths::DB_DIR).expect("could not create database path");
    std::fs::create_dir_all(&*util::paths::LOGS_DIR).expect("could not create logs path");
    std::fs::create_dir_all(&*util::paths::TEMP_DIR).expect("could not create tmp path");
}

fn init_logger() {
    if stdout_is_a_pty() {
        init_stdout_logger();
    } else {
        let level = LevelFilter::Info;

        // Prevent log file from becoming too large.
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;
        const MAX_LOG_BYTES: u64 = MIB;
        if std::fs::metadata(&*paths::LOG).map_or(false, |metadata| metadata.len() > MAX_LOG_BYTES)
        {
            let _ = std::fs::rename(&*paths::LOG, &*paths::OLD_LOG);
        }

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&*paths::LOG)
        {
            Ok(log_file) => {
                let config = ConfigBuilder::new()
                    .set_time_format_str("%Y-%m-%dT%T%:z")
                    .set_time_to_local(true)
                    .build();

                simplelog::WriteLogger::init(level, config, log_file)
                    .expect("could not initialize logger");
            }
            Err(err) => {
                init_stdout_logger();
                log::error!(
                    "could not open log file, defaulting to stdout logging: {}",
                    err
                );
            }
        }
    }
}

fn init_stdout_logger() {
    Builder::new()
        .parse_default_env()
        .format(|buf, record| {
            use env_logger::fmt::Color;

            let subtle = buf
                .style()
                .set_color(Color::Black)
                .set_intense(true)
                .clone();
            write!(buf, "{}", subtle.value("["))?;
            write!(
                buf,
                "{} ",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z")
            )?;
            write!(buf, "{:<5}", buf.default_styled_level(record.level()))?;
            if let Some(path) = record.module_path() {
                write!(buf, " {}", path)?;
            }
            write!(buf, "{}", subtle.value("]"))?;
            writeln!(buf, " {}", record.args())
        })
        .init();
}

#[derive(Serialize, Deserialize)]
struct LocationData {
    file: String,
    line: u32,
}

#[derive(Serialize, Deserialize)]
struct Panic {
    thread: String,
    payload: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    location_data: Option<LocationData>,
    backtrace: Vec<String>,
    app_version: String,
    os_name: String,
    os_version: Option<String>,
    architecture: String,
    panicked_on: i64,
}

#[derive(Serialize)]
struct PanicRequest {
    panic: Panic,
}

static PANIC_COUNT: AtomicU32 = AtomicU32::new(0);

fn init_panic_hook(app: &App) {
    let is_pty = stdout_is_a_pty();
    let app_metadata = app.metadata();

    panic::set_hook(Box::new(move |info| {
        let prior_panic_count = PANIC_COUNT.fetch_add(1, Ordering::SeqCst);
        if prior_panic_count > 0 {
            // Give the panic-ing thread time to write the panic file
            loop {
                std::thread::yield_now();
            }
        }

        let thread = thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");

        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.clone()))
            .unwrap_or_else(|| "Box<Any>".to_string());

        let app_version = if let Some(version) = app_metadata.app_version {
            version.to_string()
        } else {
            option_env!("CARGO_PKG_VERSION")
                .unwrap_or("dev")
                .to_string()
        };

        let backtrace = Backtrace::new();
        let mut backtrace = backtrace
            .frames()
            .iter()
            .flat_map(|frame| {
                frame
                    .symbols()
                    .iter()
                    .filter_map(|frame| Some(format!("{:#}", frame.name()?)))
            })
            .collect::<Vec<_>>();

        // Strip out leading stack frames for rust panic-handling.
        if let Some(ix) = backtrace
            .iter()
            .position(|name| name == "rust_begin_unwind")
        {
            backtrace.drain(0..=ix);
        }

        let panic_data = Panic {
            thread: thread_name.into(),
            payload,
            location_data: info.location().map(|location| LocationData {
                file: location.file().into(),
                line: location.line(),
            }),
            app_version: app_version.to_string(),
            os_name: app_metadata.os_name.into(),
            os_version: app_metadata
                .os_version
                .as_ref()
                .map(SemanticVersion::to_string),
            architecture: env::consts::ARCH.into(),
            panicked_on: Utc::now().timestamp_millis(),
            backtrace,
        };

        if let Some(panic_data_json) = serde_json::to_string_pretty(&panic_data).log_err() {
            log::error!("{}", panic_data_json);
        }

        if !is_pty {
            if let Some(panic_data_json) = serde_json::to_string(&panic_data).log_err() {
                let timestamp = chrono::Utc::now().format("%Y_%m_%d %H_%M_%S").to_string();
                let panic_file_path = paths::LOGS_DIR.join(format!("zed-{}.panic", timestamp));
                let panic_file = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&panic_file_path)
                    .log_err();
                if let Some(mut panic_file) = panic_file {
                    writeln!(&mut panic_file, "{}", panic_data_json).log_err();
                    panic_file.flush().log_err();
                }
            }
        }

        std::process::abort();
    }));
}

async fn load_login_shell_environment() -> Result<()> {
    let marker = "ZED_LOGIN_SHELL_START";
    let shell = env::var("SHELL").context(
        "SHELL environment variable is not assigned so we can't source login environment variables",
    )?;

    // If possible, we want to `cd` in the user's `$HOME` to trigger programs
    // such as direnv, asdf, mise, ... to adjust the PATH. These tools often hook
    // into shell's `cd` command (and hooks) to manipulate env.
    // We do this so that we get the env a user would have when spawning a shell
    // in home directory.
    let shell_cmd_prefix = std::env::var_os("HOME")
        .and_then(|home| home.into_string().ok())
        .map(|home| format!("cd '{home}';"));

    // The `exit 0` is the result of hours of debugging, trying to find out
    // why running this command here, without `exit 0`, would mess
    // up signal process for our process so that `ctrl-c` doesn't work
    // anymore.
    // We still don't know why `$SHELL -l -i -c '/usr/bin/env -0'`  would
    // do that, but it does, and `exit 0` helps.
    let shell_cmd = format!(
        "{}echo {marker}; /usr/bin/env -0; exit 0;",
        shell_cmd_prefix.as_deref().unwrap_or("")
    );

    let output = Command::new(&shell)
        .args(["-l", "-i", "-c", &shell_cmd])
        .output()
        .await
        .context("failed to spawn login shell to source login environment variables")?;
    if !output.status.success() {
        Err(anyhow!("login shell exited with error"))?;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    if let Some(env_output_start) = stdout.find(marker) {
        let env_output = &stdout[env_output_start + marker.len()..];
        for line in env_output.split_terminator('\0') {
            if let Some(separator_index) = line.find('=') {
                let key = &line[..separator_index];
                let value = &line[separator_index + 1..];
                env::set_var(key, value);
            }
        }
        log::info!(
            "set environment variables from shell:{}, path:{}",
            shell,
            env::var("PATH").unwrap_or_default(),
        );
    }

    Ok(())
}

fn stdout_is_a_pty() -> bool {
    std::env::var(FORCE_CLI_MODE_ENV_VAR_NAME).ok().is_none() && std::io::stdout().is_terminal()
}

#[derive(Parser, Debug)]
#[command(name = "zed", disable_version_flag = true)]
struct Args {
    /// A sequence of space-separated paths or urls that you want to open.
    ///
    /// Use `path:line:row` syntax to open a file at a specific location.
    /// Non-existing paths and directories will ignore `:line:row` suffix.
    ///
    /// URLs can either be file:// or zed:// scheme, or relative to https://zed.dev.
    paths_or_urls: Vec<String>,
}

fn parse_url_arg(arg: &str) -> Result<String> {
    match std::fs::canonicalize(Path::new(&arg)) {
        Ok(path) => Ok(format!("file://{}", path.to_string_lossy())),
        Err(error) => {
            if arg.starts_with("file://") || arg.starts_with("zed-cli://") {
                Ok(arg.into())
            } else {
                Err(anyhow!("error parsing path argument: {}", error))
            }
        }
    }
}

fn load_embedded_fonts(cx: &AppContext) {
    let asset_source = cx.asset_source();
    let font_paths = asset_source.list("fonts").unwrap();
    let embedded_fonts = Mutex::new(Vec::new());
    let executor = cx.background_executor();

    executor.block(executor.scoped(|scope| {
        for font_path in &font_paths {
            if !font_path.ends_with(".ttf") {
                continue;
            }

            scope.spawn(async {
                let font_bytes = asset_source.load(font_path).unwrap();
                embedded_fonts.lock().push(font_bytes);
            });
        }
    }));

    cx.text_system()
        .add_fonts(embedded_fonts.into_inner())
        .unwrap();
}

/// Spawns a background task to load the user themes from the themes directory.
fn load_user_themes_in_background(fs: Arc<dyn fs::Fs>, cx: &mut AppContext) {
    cx.spawn({
        let fs = fs.clone();
        |cx| async move {
            if let Some(theme_registry) =
                cx.update(|cx| ThemeRegistry::global(cx).clone()).log_err()
            {
                let themes_dir = paths::THEMES_DIR.as_ref();
                match fs
                    .metadata(themes_dir)
                    .await
                    .ok()
                    .flatten()
                    .map(|m| m.is_dir)
                {
                    Some(is_dir) => {
                        anyhow::ensure!(is_dir, "Themes dir path {themes_dir:?} is not a directory")
                    }
                    None => {
                        fs.create_dir(themes_dir).await.with_context(|| {
                            format!("Failed to create themes dir at path {themes_dir:?}")
                        })?;
                    }
                }
                theme_registry.load_user_themes(themes_dir, fs).await?;
                cx.update(|cx| ThemeSettings::reload_current_theme(cx))?;
            }
            anyhow::Ok(())
        }
    })
    .detach_and_log_err(cx);
}

/// Spawns a background task to watch the themes directory for changes.
fn watch_themes(fs: Arc<dyn fs::Fs>, cx: &mut AppContext) {
    use std::time::Duration;
    cx.spawn(|cx| async move {
        let mut events = fs
            .watch(&paths::THEMES_DIR.clone(), Duration::from_millis(100))
            .await;

        while let Some(paths) = events.next().await {
            for path in paths {
                if fs.metadata(&path).await.ok().flatten().is_some() {
                    if let Some(theme_registry) =
                        cx.update(|cx| ThemeRegistry::global(cx).clone()).log_err()
                    {
                        if let Some(()) = theme_registry
                            .load_user_theme(&path, fs.clone())
                            .await
                            .log_err()
                        {
                            cx.update(|cx| ThemeSettings::reload_current_theme(cx))
                                .log_err();
                        }
                    }
                }
            }
        }
    })
    .detach()
}

#[cfg(debug_assertions)]
fn watch_file_types(fs: Arc<dyn fs::Fs>, cx: &mut AppContext) {
    use std::time::Duration;

    use gpui::BorrowAppContext;

    let path = {
        let p = Path::new("assets/icons/file_icons/file_types.json");
        let Ok(full_path) = p.canonicalize() else {
            return;
        };
        full_path
    };

    cx.spawn(|cx| async move {
        let mut events = fs.watch(path.as_path(), Duration::from_millis(100)).await;
        while (events.next().await).is_some() {
            cx.update(|cx| {
                cx.update_global(|file_types, _| {
                    *file_types = project_panel::file_associations::FileAssociations::new(Assets);
                });
            })
            .ok();
        }
    })
    .detach()
}

#[cfg(not(debug_assertions))]
fn watch_file_types(_fs: Arc<dyn fs::Fs>, _cx: &mut AppContext) {}
