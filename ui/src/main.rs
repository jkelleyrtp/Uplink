#![cfg_attr(feature = "production_mode", windows_subsystem = "windows")]
// the above macro will make uplink be a "window" application instead of a  "console" application for Windows.

use clap::Parser;
use common::icons::outline::Shape as Icon;
use common::icons::Icon as IconElement;
use common::language::get_local_text;
use common::warp_runner::BlinkCmd;

use common::notifications::{NotificationAction, NOTIFICATION_LISTENER};
use common::warp_runner::ui_adapter::MessageEvent;
use common::warp_runner::WarpEvent;
use common::{get_extras_dir, warp_runner, LogProfile, STATIC_ARGS, WARP_CMD_CH, WARP_EVENT_CH};
use components::calldialog::CallDialog;
use dioxus::prelude::*;
use dioxus_desktop::tao::dpi::LogicalSize;
use dioxus_desktop::tao::event::WindowEvent;
use dioxus_desktop::tao::menu::AboutMetadata;
use dioxus_desktop::Config;
use dioxus_desktop::{tao, use_window};
use extensions::UplinkExtension;
use futures::channel::oneshot;
use futures::StreamExt;
use kit::components::context_menu::{ContextItem, ContextMenu};
use kit::components::modal::Modal;
use kit::components::nav::Route as UIRoute;
use kit::components::topbar_controls::Topbar_Controls;
use kit::components::user_image::UserImage;
use kit::components::user_image_group::UserImageGroup;
use kit::elements::button::Button;
use kit::elements::Appearance;
use layouts::friends::FriendRoute;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use once_cell::sync::Lazy;
use overlay::{make_config, OverlayDom};
use utils::build_user_from_identity;
use uuid::Uuid;

use std::collections::HashMap;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;
use warp::multipass;

use std::sync::Arc;
use tao::menu::{MenuBar as Menu, MenuItem};
use tao::window::WindowBuilder;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, Duration};
use warp::logging::tracing::log::{self, LevelFilter};

use dioxus_desktop::use_wry_event_handler;
use dioxus_desktop::wry::application::event::Event as WryEvent;

use crate::components::debug_logger::DebugLogger;
use crate::components::toast::Toast;
use crate::components::topbar::release_info::Release_Info;
use crate::layouts::create_account::CreateAccountLayout;
use crate::layouts::friends::FriendsLayout;
use crate::layouts::loading::LoadingLayout;
use crate::layouts::settings::SettingsLayout;
use crate::layouts::storage::{FilesLayout, DRAG_EVENT};
use crate::layouts::unlock::UnlockLayout;

use crate::utils::auto_updater::{
    DownloadProgress, DownloadState, SoftwareDownloadCmd, SoftwareUpdateCmd,
};

use crate::utils::build_participants;
use crate::window_manager::WindowManagerCmdChannels;
use crate::{components::chat::RouteInfo, layouts::chat::ChatLayout};
use common::{
    state::{storage, ui::WindowMeta, Action, State},
    warp_runner::{ConstellationCmd, RayGunCmd, WarpCmd},
};
use dioxus_router::*;
use std::panic;

use kit::STYLE as UIKIT_STYLES;
pub const APP_STYLE: &str = include_str!("./compiled_styles.css");
mod components;
mod extension_browser;
mod layouts;
mod logger;
mod overlay;
mod utils;
mod window_manager;

pub static OPEN_DYSLEXIC: &str = include_str!("./open-dyslexic.css");

pub const PRISM_SCRIPT: &str = include_str!("../extra/assets/scripts/prism.js");
pub const PRISM_STYLE: &str = include_str!("../extra/assets/styles/prism.css");
pub const PRISM_THEME: &str = include_str!("../extra/assets/styles/prism-one-dark.css");

// used to close the popout player, among other things
pub static WINDOW_CMD_CH: Lazy<WindowManagerCmdChannels> = Lazy::new(|| {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    WindowManagerCmdChannels {
        tx,
        rx: Arc::new(Mutex::new(rx)),
    }
});

pub struct UplinkRoutes<'a> {
    pub loading: &'a str,
    pub chat: &'a str,
    pub friends: &'a str,
    pub files: &'a str,
    pub settings: &'a str,
}

pub static UPLINK_ROUTES: UplinkRoutes = UplinkRoutes {
    loading: "/",
    chat: "/chat",
    friends: "/friends",
    files: "/files",
    settings: "/settings",
};

// serve as a sort of router while the user logs in]
#[allow(clippy::large_enum_variant)]
#[derive(PartialEq, Eq)]
pub enum AuthPages {
    Unlock,
    CreateAccount,
    Success(multipass::identity::Identity),
}

fn main() {
    // Attempts to increase the file desc limit on unix-like systems
    // Note: Will be changed out in the future
    if fdlimit::raise_fd_limit().is_none() {}
    // configure logging
    let args = common::Args::parse();
    let max_log_level = if let Some(profile) = args.profile {
        match profile {
            LogProfile::Debug => {
                logger::set_write_to_stdout(true);
                LevelFilter::Debug
            }
            LogProfile::Trace => {
                logger::set_display_trace(true);
                logger::set_write_to_stdout(true);
                LevelFilter::Trace
            }
            LogProfile::Trace2 => {
                logger::set_display_warp(true);
                logger::set_display_trace(true);
                logger::set_write_to_stdout(true);
                LevelFilter::Trace
            }
            _ => LevelFilter::Debug,
        }
    } else {
        LevelFilter::Debug
    };
    logger::init_with_level(max_log_level).expect("failed to init logger");
    log::debug!("starting uplink");
    panic::set_hook(Box::new(|panic_info| {
        let intro = match panic_info.payload().downcast_ref::<&str>() {
            Some(s) => format!("panic occurred: {s:?}"),
            None => "panic occurred".into(),
        };
        let location = match panic_info.location() {
            Some(loc) => format!(" at file {}, line {}", loc.file(), loc.line()),
            None => "".into(),
        };

        let logs = logger::dump_logs();
        let crash_report = format!("{intro}{location}\n{logs}\n");
        println!("{crash_report}");
    }));

    // Initializes the cache dir if needed
    std::fs::create_dir_all(&STATIC_ARGS.uplink_path).expect("Error creating Uplink directory");
    std::fs::create_dir_all(&STATIC_ARGS.warp_path).expect("Error creating Warp directory");
    std::fs::create_dir_all(&STATIC_ARGS.themes_path).expect("error creating themes directory");
    std::fs::create_dir_all(&STATIC_ARGS.fonts_path).expect("error fonts themes directory");

    let window = get_window_builder(true, true);

    let config = Config::new()
        .with_window(window)
        .with_custom_index(
            r#"
<!doctype html>
<html>
<script src="https://cdn.jsdelivr.net/npm/interactjs/dist/interact.min.js"></script>
<body style="background-color:rgba(0,0,0,0);"><div id="main"></div></body>
</html>"#
                .to_string(),
        )
        .with_file_drop_handler(|_w, drag_event| {
            log::info!("Drag Event: {:?}", drag_event);
            *DRAG_EVENT.write() = drag_event;
            true
        });

    let config = if cfg!(target_os = "windows") && STATIC_ARGS.production_mode {
        let webview_data_dir = STATIC_ARGS.dot_uplink.join("tmp");
        std::fs::create_dir_all(&webview_data_dir).expect("error creating webview data directory");
        config.with_data_directory(webview_data_dir)
    } else {
        config
    };

    dioxus_desktop::launch_cfg(bootstrap, config)
}

pub fn get_window_builder(with_predefined_size: bool, with_menu: bool) -> WindowBuilder {
    let mut main_menu = Menu::new();
    let mut app_menu = Menu::new();
    let mut edit_menu = Menu::new();
    let mut window_menu = Menu::new();

    app_menu.add_native_item(MenuItem::About(
        String::from("Uplink"),
        AboutMetadata::default(),
    ));
    app_menu.add_native_item(MenuItem::Quit);
    // add native shortcuts to `edit_menu` menu
    // in macOS native item are required to get keyboard shortcut
    // to works correctly
    edit_menu.add_native_item(MenuItem::Undo);
    edit_menu.add_native_item(MenuItem::Redo);
    edit_menu.add_native_item(MenuItem::Separator);
    edit_menu.add_native_item(MenuItem::Cut);
    edit_menu.add_native_item(MenuItem::Copy);
    edit_menu.add_native_item(MenuItem::Paste);
    edit_menu.add_native_item(MenuItem::SelectAll);

    window_menu.add_native_item(MenuItem::Minimize);
    window_menu.add_native_item(MenuItem::Zoom);
    window_menu.add_native_item(MenuItem::Separator);
    window_menu.add_native_item(MenuItem::ShowAll);
    window_menu.add_native_item(MenuItem::EnterFullScreen);
    window_menu.add_native_item(MenuItem::Separator);
    window_menu.add_native_item(MenuItem::CloseWindow);

    main_menu.add_submenu("Uplink", true, app_menu);
    main_menu.add_submenu("Edit", true, edit_menu);
    main_menu.add_submenu("Window", true, window_menu);

    let title = get_local_text("uplink");

    #[allow(unused_mut)]
    let mut window = WindowBuilder::new()
        .with_title(title)
        .with_resizable(true)
        // We start the min inner size smaller because the prelude pages like unlock can be rendered much smaller.
        .with_min_inner_size(LogicalSize::new(300.0, 350.0));

    if with_predefined_size {
        window = window.with_inner_size(LogicalSize::new(950.0, 600.0));
    }

    if with_menu {
        #[cfg(target_os = "macos")]
        {
            window = window.with_menu(main_menu)
        }
    }

    #[cfg(target_os = "macos")]
    {
        use dioxus_desktop::tao::platform::macos::WindowBuilderExtMacOS;

        window = window
            .with_has_shadow(true)
            .with_transparent(true)
            .with_fullsize_content_view(true)
            .with_titlebar_transparent(true)
            .with_title("")
    }

    #[cfg(not(target_os = "macos"))]
    {
        window = window.with_decorations(false).with_transparent(true);
    }
    window
}

// start warp_runner and ensure the user is logged in
fn bootstrap(cx: Scope) -> Element {
    log::trace!("rendering bootstrap");

    // warp_runner must be started from within a tokio reactor
    // store in a use_ref to make it not get dropped
    let warp_runner = use_ref(cx, warp_runner::WarpRunner::new);
    warp_runner.write_silent().run();

    // make the window smaller while the user authenticates
    let desktop = use_window(cx);
    desktop.set_inner_size(LogicalSize {
        width: 500.0,
        height: 350.0,
    });

    cx.render(rsx!(crate::auth_page_manager {}))
}

// Uplink's Router depends on State, which can't be loaded until the user logs in.
// don't see a way to replace the router
// so instead use a Prop to determine which page to render
// after the user logs in, app_bootstrap loads Uplink as normal.
fn auth_page_manager(cx: Scope) -> Element {
    let page = use_state(cx, || AuthPages::Unlock);
    let pin = use_ref(cx, String::new);
    cx.render(rsx!(match &*page.current() {
        AuthPages::Success(ident) => rsx!(app_bootstrap {
            identity: ident.clone()
        }),
        _ => rsx!(auth_wrapper {
            page: page.clone(),
            pin: pin.clone()
        }),
    }))
}

#[allow(unused_assignments)]
#[inline_props]
fn auth_wrapper(cx: Scope, page: UseState<AuthPages>, pin: UseRef<String>) -> Element {
    log::trace!("rendering auth wrapper");
    let desktop = use_window(cx);
    let theme = "";

    cx.render(rsx! (
        style { "{UIKIT_STYLES} {APP_STYLE} {theme}" },
        div {
            id: "app-wrap",
            div {
                class: "titlebar disable-select",
                id: if cfg!(target_os = "macos") {""}  else {"lockscreen-controls"},
                onmousedown: move |_| { desktop.drag(); },
                Topbar_Controls {},
            },
            match *page.current() {
                AuthPages::Unlock => rsx!(UnlockLayout { page: page.clone(), pin: pin.clone() }),
                AuthPages::CreateAccount => rsx!(CreateAccountLayout { page: page.clone(), pin: pin.clone() }),
                _ => panic!("invalid page")
            }
        }
    ))
}

fn get_extensions() -> Result<HashMap<String, UplinkExtension>, Box<dyn std::error::Error>> {
    fs::create_dir_all(&STATIC_ARGS.extensions_path)?;
    let mut extensions = HashMap::new();

    let mut add_to_extensions = |dir: fs::ReadDir| -> Result<(), Box<dyn std::error::Error>> {
        for entry in dir {
            let path = entry?.path();
            log::debug!("Found extension: {:?}", path);

            match UplinkExtension::new(path.clone()) {
                Ok(ext) => {
                    if ext.cargo_version() != extensions::CARGO_VERSION
                        || ext.rustc_version() != extensions::RUSTC_VERSION
                    {
                        log::warn!("failed to load extension: {:?} due to rustc/cargo version mismatch. cargo version: {}, rustc version: {}", &path, ext.cargo_version(), ext.rustc_version());
                        continue;
                    }
                    log::debug!("Loaded extension: {:?}", &path);
                    extensions.insert(ext.details().meta.name.into(), ext);
                }
                Err(e) => {
                    log::error!("Error loading extension: {:?}", e);
                }
            }
        }

        Ok(())
    };

    let user_extension_dir = fs::read_dir(&STATIC_ARGS.extensions_path)?;
    add_to_extensions(user_extension_dir)?;

    if STATIC_ARGS.production_mode {
        let uplink_extenions_path = common::get_extensions_dir()?;
        let uplink_extensions_dir = fs::read_dir(uplink_extenions_path)?;
        add_to_extensions(uplink_extensions_dir)?;
    }

    Ok(extensions)
}

// called at the end of the auth flow
#[inline_props]
pub fn app_bootstrap(cx: Scope, identity: multipass::identity::Identity) -> Element {
    log::trace!("rendering app_bootstrap");
    let mut state = State::load();

    if STATIC_ARGS.use_mock {
        assert!(state.initialized);
    } else {
        state.set_own_identity(identity.clone().into());
    }

    let desktop = use_window(cx);
    // TODO: This overlay needs to be fixed in windows
    if cfg!(not(target_os = "windows")) && state.configuration.general.enable_overlay {
        let overlay_test = VirtualDom::new(OverlayDom);
        let window = desktop.new_window(overlay_test, make_config());
        state.ui.overlays.push(window);
    }

    let size = desktop.webview.inner_size();
    // Update the window metadata now that we've created a window
    let window_meta = WindowMeta {
        focused: desktop.is_focused(),
        maximized: desktop.is_maximized(),
        minimized: desktop.is_minimized(),
        minimal_view: size.width < 1200, // todo: why is it that on Linux, checking if desktop.inner_size().width < 600 is true?
    };
    state.ui.metadata = window_meta;

    use_shared_state_provider(cx, || state);
    use_shared_state_provider(cx, DownloadState::default);

    cx.render(rsx!(crate::app {}))
}

fn app(cx: Scope) -> Element {
    log::trace!("rendering app");
    let desktop = use_window(cx);
    let state = use_shared_state::<State>(cx)?;
    let download_state = use_shared_state::<DownloadState>(cx)?;

    let prism_path = if STATIC_ARGS.production_mode {
        if cfg!(target_os = "windows") {
            STATIC_ARGS.dot_uplink.join("prism_langs")
        } else {
            get_extras_dir().unwrap_or_default().join("prism_langs")
        }
    } else {
        PathBuf::from("ui").join("extra").join("prism_langs")
    };
    let prism_autoloader_script = format!(
        r"Prism.plugins.autoloader.languages_path = '{}';",
        prism_path.to_string_lossy()
    );

    // don't fetch stuff from warp when using mock data
    let items_init = use_ref(cx, || STATIC_ARGS.use_mock);

    let mut font_style = String::new();
    if let Some(font) = state.read().ui.font.clone() {
        font_style = format!(
            "
        @font-face {{
            font-family: CustomFont;
            src: url('{}');
        }}
        body,
        html {{
            font-family: CustomFont, sans-serif;
        }}
        ",
            font.path
        );
    }

    // this gets rendered at the bottom. this way you don't have to scroll past all the use_futures to see what this function renders
    let main_element = {
        // render the Uplink app
        let open_dyslexic = if state.read().configuration.general.dyslexia_support {
            OPEN_DYSLEXIC
        } else {
            ""
        };

        let font_scale = format!(
            "html {{ font-size: {}rem; }}",
            state.read().settings.font_scale()
        );

        let theme = state
            .read()
            .ui
            .theme
            .as_ref()
            .map(|theme| theme.styles.clone())
            .unwrap_or_default();

        rsx! (
            style { "{UIKIT_STYLES} {APP_STYLE} {PRISM_STYLE} {PRISM_THEME} {theme} {font_style} {open_dyslexic} {font_scale}" },
            div {
                id: "app-wrap",
                get_titlebar{},
                get_toasts{},
                get_call_dialog{},
                get_router{},
                get_logger{},
            },
            script { "{PRISM_SCRIPT}" },
            script { "{prism_autoloader_script}" },
        )
    };

    // use_coroutine for software update

    // updates the UI
    let updater_ch = use_coroutine(cx, |mut rx: UnboundedReceiver<SoftwareUpdateCmd>| {
        to_owned![download_state];
        async move {
            while let Some(mut ch) = rx.next().await {
                while let Some(percent) = ch.0.recv().await {
                    if percent >= download_state.read().progress + 5_f32 {
                        download_state.write().progress = percent;
                    }
                }
                download_state.write().stage = DownloadProgress::Finished;
            }
        }
    });

    // receives a download command
    let _download_ch = use_coroutine(cx, |mut rx: UnboundedReceiver<SoftwareDownloadCmd>| {
        to_owned![updater_ch];
        async move {
            while let Some(dest) = rx.next().await {
                let (tx, rx) = mpsc::unbounded_channel::<f32>();
                updater_ch.send(SoftwareUpdateCmd(rx));
                match utils::auto_updater::download_update(dest.0.clone(), tx).await {
                    Ok(downloaded_version) => {
                        log::debug!("downloaded version {downloaded_version}");
                    }
                    Err(e) => {
                        log::error!("failed to download update: {e}");
                    }
                }
            }
        }
    });

    // `use_future`s
    // all of Uplinks periodic tasks are located here. it's a lot to read but
    // it's better to have them in one place. this makes it a lot easier to find them.
    // there are 2 categories of tasks: warp tasks and UI tasks
    //
    // warp tasks
    // handle warp events
    // initialize friends: load from warp and store in State
    // initialize conversations: same
    //
    // UI tasks
    // clear toasts
    // update message timestamps
    // control child windows
    // clear typing indicator
    //
    // misc
    // when a task requires the UI be updated, `needs_update` is set.
    // when mock data is used, friends and conversations are generated randomly,
    //     not loaded from Warp. however, warp_runner continues to operate normally.
    //

    // There is currently an issue in Tauri/Wry where the window size is not reported properly.
    // Thus we bind to the resize event itself and update the size from the webview.
    let webview = desktop.webview.clone();
    use_wry_event_handler(cx, {
        to_owned![state, desktop];
        move |event, _| match event {
            WryEvent::WindowEvent {
                event: WindowEvent::Focused(focused),
                ..
            } => {
                //log::trace!("FOCUS CHANGED {:?}", *focused);
                if state.read().ui.metadata.focused != *focused {
                    state.write().ui.metadata.focused = *focused;

                    if *focused {
                        state.write().ui.notifications.clear_badge();
                        let _ = state.write().save();
                    }
                }
            }
            WryEvent::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => state
                .write()
                .mutate(Action::ClearAllPopoutWindows(desktop.clone())),
            WryEvent::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } => {
                let size = webview.inner_size();
                //log::trace!(
                //    "Resized - PhysicalSize: {:?}, Minimal: {:?}",
                //    size,
                //    size.width < 1200
                //);

                let metadata = state.read().ui.metadata.clone();
                let new_metadata = WindowMeta {
                    minimal_view: size.width < 600,
                    ..metadata
                };
                if metadata != new_metadata {
                    state.write().ui.sidebar_hidden = new_metadata.minimal_view;
                    state.write().ui.metadata = new_metadata;
                }
            }
            _ => {}
        }
    });

    // update state in response to warp events
    use_future(cx, (), |_| {
        to_owned![cx, state];
        let schedule: Arc<dyn Fn(ScopeId) + Send + Sync> = cx.schedule_update_any();
        async move {
            // don't process warp events until friends and chats have been loaded
            while !state.read().initialized {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let warp_event_rx = WARP_EVENT_CH.rx.clone();
            log::trace!("starting warp_runner use_future");
            // it should be sufficient to lock once at the start of the use_future. this is the only place the channel should be read from. in the off change that
            // the future restarts (it shouldn't), the lock should be dropped and this wouldn't block.
            let mut ch = warp_event_rx.lock().await;
            while let Some(evt) = ch.recv().await {
                // Update only relevant components for attachment progress events
                if let WarpEvent::Message(MessageEvent::AttachmentProgress {
                    progress,
                    conversation_id,
                    msg,
                }) = evt
                {
                    state
                        .write_silent()
                        .update_outgoing_messages(conversation_id, msg, progress);
                    let read = state.read();
                    if read
                        .get_active_chat()
                        .map(|c| c.id.eq(&conversation_id))
                        .unwrap_or_default()
                    {
                        //Update the component only instead of whole state
                        if let Some(v) = read.scope_ids.pending_message_component {
                            schedule(ScopeId(v))
                        }
                    }
                } else {
                    state.write().process_warp_event(evt);
                }
            }
        }
    });

    // focus handler for notifications
    use_future(cx, (), |_| {
        to_owned![desktop];
        async move {
            let channel = common::notifications::FOCUS_SCHEDULER.rx.clone();
            let mut ch = channel.lock().await;
            while (ch.recv().await).is_some() {
                desktop.set_focus();
            }
        }
    });

    // clear toasts
    use_future(cx, (), |_| {
        to_owned![state];
        async move {
            loop {
                sleep(Duration::from_secs(1)).await;
                if !state.read().has_toasts() {
                    continue;
                }
                log::trace!("decrement toasts");
                state.write().decrement_toasts();
            }
        }
    });

    // clear typing indicator
    use_future(cx, (), |_| {
        to_owned![state];
        async move {
            loop {
                sleep(Duration::from_secs(STATIC_ARGS.typing_indicator_timeout)).await;
                if state.write_silent().clear_typing_indicator(Instant::now()) {
                    log::trace!("clear typing indicator");
                    state.write();
                }
            }
        }
    });

    // periodically refresh message timestamps and friend's status messages
    use_future(cx, (), |_| {
        to_owned![state];
        async move {
            loop {
                // simply triggering an update will refresh the message timestamps
                sleep(Duration::from_secs(60)).await;
                log::trace!("refresh timestamps");
                state.write();
            }
        }
    });

    // check for updates
    use_future(cx, (), |_| {
        to_owned![state];
        async move {
            loop {
                let latest_release = match utils::auto_updater::check_for_release().await {
                    Ok(opt) => match opt {
                        Some(r) => r,
                        None => {
                            sleep(Duration::from_secs(3600 * 24)).await;
                            continue;
                        }
                    },
                    Err(e) => {
                        log::error!("failed to check for release: {e}");
                        sleep(Duration::from_secs(3600 * 24)).await;
                        continue;
                    }
                };
                if state.read().settings.update_dismissed == Some(latest_release.tag_name.clone()) {
                    sleep(Duration::from_secs(3600 * 24)).await;
                    continue;
                }
                state.write().update_available(latest_release.tag_name);
                sleep(Duration::from_secs(3600 * 24)).await;
            }
        }
    });

    // control child windows
    use_future(cx, (), |_| {
        to_owned![desktop, state];
        async move {
            let window_cmd_rx = WINDOW_CMD_CH.rx.clone();
            let mut ch = window_cmd_rx.lock().await;
            while let Some(cmd) = ch.recv().await {
                window_manager::handle_cmd(state.clone(), cmd, desktop.clone()).await;
            }
        }
    });

    // init state from warp
    // also init extensions
    use_future(cx, (), |_| {
        to_owned![state];
        async move {
            if state.read().initialized {
                return;
            }

            // this is technically bad because it blocks the async runtime
            match get_extensions() {
                Ok(ext) => {
                    for (name, extension) in ext {
                        state.write().ui.extensions.insert(name, extension);
                    }
                }
                Err(e) => {
                    log::error!("failed to get extensions: {e}");
                }
            }
            log::debug!(
                "Loaded {} extensions.",
                state.read().ui.extensions.values().count()
            );

            let warp_cmd_tx = WARP_CMD_CH.tx.clone();
            let res = loop {
                let (tx, rx) = oneshot::channel();
                if let Err(e) =
                    warp_cmd_tx.send(WarpCmd::RayGun(RayGunCmd::InitializeWarp { rsp: tx }))
                {
                    log::error!("failed to send command to initialize warp {}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                let res = rx.await.expect("failed to get response from warp_runner");

                let res = match res {
                    Ok(r) => r,
                    Err(e) => {
                        log::error!("failed to initialize warp: {}", e);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                break res;
            };

            state
                .write()
                .init_warp(res.friends, res.chats, res.converted_identities);
        }
    });

    // initialize files
    use_future(cx, (), |_| {
        to_owned![items_init, state];
        async move {
            if *items_init.read() {
                return;
            }
            let warp_cmd_tx = WARP_CMD_CH.tx.clone();
            let (tx, rx) = oneshot::channel::<Result<storage::Storage, warp::error::Error>>();

            if let Err(e) = warp_cmd_tx.send(WarpCmd::Constellation(
                ConstellationCmd::GetItemsFromCurrentDirectory { rsp: tx },
            )) {
                log::error!("failed to initialize Files {}", e);
                return;
            }

            let res = rx.await.expect("failed to get response from warp_runner");

            log::trace!("init items");
            match res {
                Ok(storage) => state.write().storage = storage,
                Err(e) => {
                    log::error!("init items failed: {}", e);
                }
            }

            *items_init.write() = true;
        }
    });

    // detect when new extensions are placed in the "extensions" folder, and load them.
    use_future(cx, (), |_| {
        to_owned![state];
        async move {
            let (tx, mut rx) = futures::channel::mpsc::unbounded();
            let mut watcher = match RecommendedWatcher::new(
                move |res| {
                    let _ = tx.unbounded_send(res);
                },
                notify::Config::default().with_poll_interval(Duration::from_secs(1)),
            ) {
                Ok(watcher) => watcher,
                Err(e) => {
                    log::error!("{e}");
                    return;
                }
            };

            // Add a path to be watched. All files and directories at that path and
            // below will be monitored for changes.
            if let Err(e) = watcher.watch(
                STATIC_ARGS.extensions_path.as_path(),
                RecursiveMode::Recursive,
            ) {
                log::error!("{e}");
                return;
            }

            while let Some(event) = rx.next().await {
                let event = match event {
                    Ok(event) => event,
                    Err(e) => {
                        log::error!("{e}");
                        continue;
                    }
                };

                log::debug!("{event:?}");
                match get_extensions() {
                    Ok(ext) => {
                        state.write().mutate(Action::RegisterExtensions(ext));
                    }
                    Err(e) => {
                        log::error!("failed to get extensions: {e}");
                    }
                }
            }
        }
    });

    cx.render(main_element)
}

fn get_update_icon(cx: Scope) -> Element {
    log::trace!("rendering get_update_icon");
    let state = use_shared_state::<State>(cx)?;
    let download_state = use_shared_state::<DownloadState>(cx)?;
    let desktop = use_window(cx);
    let _download_ch = use_coroutine_handle::<SoftwareDownloadCmd>(cx)?;

    let new_version = match state.read().settings.update_available.as_ref() {
        Some(u) => u.clone(),
        None => return cx.render(rsx!("")),
    };

    let update_msg = format!(
        "{}: {}",
        get_local_text("uplink.update-available"),
        new_version,
    );
    let downloading_msg = format!(
        "{}: {}%",
        get_local_text("uplink.update-downloading"),
        download_state.read().progress as u32
    );
    let downloaded_msg = get_local_text("uplink.update-downloaded");

    let stage = download_state.read().stage;
    match stage {
        DownloadProgress::Idle => cx.render(rsx!(
            ContextMenu {
                key: "update-available-menu",
                id: "update-available-menu".to_string(),
                items: cx.render(rsx!(
                    ContextItem {
                        aria_label: "update-menu-dismiss".into(),
                        text: get_local_text("uplink.update-menu-dismiss"),
                        onpress: move |_| {
                            state.write().mutate(Action::DismissUpdate);
                        }
                    },
                    ContextItem {
                        aria_label: "update-menu-download".into(),
                        text: get_local_text("uplink.update-menu-download"),
                        onpress: move |_| {
                            download_state.write().stage = DownloadProgress::PickFolder;

                        }
                    }
                )),
                div {
                    id: "update-available",
                    aria_label: "update-available",
                    onclick: move |_| {
                        download_state.write().stage = DownloadProgress::PickFolder;

                    },
                    IconElement {
                        icon: common::icons::solid::Shape::ArrowDownCircle,
                    },
                    "{update_msg}",
                }
            }
        )),
        DownloadProgress::PickFolder => cx.render(rsx!(get_download_modal {
            on_dismiss: move |_| {
                download_state.write().stage = DownloadProgress::Idle;
            },
            // is never used
            // on_submit: move |dest: PathBuf| {
            //     download_state.write().stage = DownloadProgress::Pending;
            //     download_state.write().destination = Some(dest.clone());
            //     download_ch.send(SoftwareDownloadCmd(dest));
            // }
        })),
        DownloadProgress::_Pending => cx.render(rsx!(div {
            id: "update-available",
            class: "topbar-item",
            aria_label: "update-available",
            "{downloading_msg}"
        })),
        DownloadProgress::Finished => {
            cx.render(rsx!(div {
                id: "update-available",
                class: "topbar-item",
                aria_label: "update-available",
                onclick: move |_| {
                    // be sure to update this before closing the app
                    state.write().mutate(Action::DismissUpdate);
                    if let Some(dest) = download_state.read().destination.clone() {
                        std::thread::spawn(move ||  {

                            let cmd = if cfg!(target_os = "windows") {
                                "explorer"
                            } else if cfg!(target_os = "linux") {
                                "xdg-open"
                            } else if cfg!(target_os = "macos") {
                                "open"
                            } else {
                               eprintln!("unknown OS type. failed to open files browser");
                               return;
                            };
                            Command::new(cmd)
                            .arg(dest)
                            .spawn()
                            .unwrap();
                        });
                        desktop.close();
                    } else {
                        log::error!("attempted to download update without download location");
                    }
                    download_state.write().destination = None;
                    download_state.write().stage = DownloadProgress::Idle;
                },
                "{downloaded_msg}"
            }))
        }
    }
}

#[inline_props]
pub fn get_download_modal<'a>(
    cx: Scope<'a>,
    //on_submit: EventHandler<'a, PathBuf>,
    on_dismiss: EventHandler<'a, ()>,
) -> Element<'a> {
    let download_location: &UseState<Option<PathBuf>> = use_state(cx, || None);

    let dl = download_location.current();
    let _disp_download_location = dl
        .as_ref()
        .clone()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();

    cx.render(rsx!(Modal {
        on_dismiss: move |_| on_dismiss.call(()),
        children: cx.render(rsx!(
            div {
            class: "download-modal disp-flex col",
            h1 {
                get_local_text("updates.title")
            },
            ul {
                class: "instruction-list",
                li {
                    get_local_text("updates.instruction1")
                },
                li {
                    Button {
                        text: get_local_text("updates.download-label"),
                        aria_label: get_local_text("updates.download-label"),
                        appearance: Appearance::Secondary,
                        onpress: |_| {
                            let _ = open::that("https://github.com/Satellite-im/Uplink/releases/latest");
                        }
                    }
                },
                li {
                    get_local_text("updates.instruction2")
                },
                li {
                    get_local_text("updates.instruction3")
                },
                li {
                    get_local_text("updates.instruction4")
                }
            },
            p {
                get_local_text("updates.instruction5")
            },
            // dl.as_ref().clone().map(|dest| rsx!(
            //     Button {
            //         text: "download installer".into(),
            //         onpress: move |_| {
            //            on_submit.call(dest.clone());
            //         }
            //     }
            // ))
        }
        ))
    }))
}

fn get_logger(cx: Scope) -> Element {
    let state = use_shared_state::<State>(cx)?;

    cx.render(rsx!(state
        .read()
        .configuration
        .developer
        .developer_mode
        .then(|| rsx!(DebugLogger {}))))
}

fn get_toasts(cx: Scope) -> Element {
    let state = use_shared_state::<State>(cx)?;
    cx.render(rsx!(state.read().ui.toast_notifications.iter().map(
        |(id, toast)| {
            rsx!(Toast {
                id: *id,
                with_title: toast.title.clone(),
                with_content: toast.content.clone(),
                icon: toast.icon.unwrap_or(Icon::InformationCircle),
                appearance: Appearance::Secondary,
            },)
        }
    )))
}

#[allow(unused_assignments)]
fn get_titlebar(cx: Scope) -> Element {
    let desktop = use_window(cx);

    cx.render(rsx!(
        div {
            class: "titlebar disable-select",
            onmousedown: move |_| { desktop.drag(); },
            Release_Info{},
            cx.render(rsx!(span {
                class: "inline-controls",
                get_update_icon{},
                Topbar_Controls {}
            })),
        },
    ))
}

enum CallDialogCmd {
    Accept(Uuid),
    Reject(Uuid),
}

fn get_call_dialog(cx: Scope) -> Element {
    let state = use_shared_state::<State>(cx)?;
    let ch = use_coroutine(cx, |mut rx| {
        to_owned![state];
        async move {
            let warp_cmd_tx = WARP_CMD_CH.tx.clone();
            while let Some(cmd) = rx.next().await {
                match cmd {
                    CallDialogCmd::Accept(id) => {
                        let (tx, rx) = oneshot::channel();
                        if let Err(_e) = warp_cmd_tx.send(WarpCmd::Blink(BlinkCmd::AnswerCall {
                            call_id: id,
                            rsp: tx,
                        })) {
                            log::error!("failed to send blink command");
                            continue;
                        }

                        match rx.await {
                            Ok(_) => {
                                state.write().mutate(Action::AnswerCall(id));
                            }
                            Err(e) => {
                                log::error!("warp_runner failed to answer call: {e}");
                            }
                        }
                    }
                    CallDialogCmd::Reject(id) => {
                        state.write().ui.call_info.reject_call(id);
                    }
                }
            }
        }
    });

    let call = match state.read().ui.call_info.active_call() {
        Some(_) => return None,
        None => match state.read().ui.call_info.pending_calls().first() {
            Some(call) => call.clone(),
            None => return None,
        },
    };
    let mut participants = state.read().get_identities(&call.participants);
    let own_id = state.read().did_key();
    participants.retain(|x| x.did_key() != own_id);

    let my_identity = build_user_from_identity(state.read().get_own_identity());

    cx.render(rsx!(CallDialog {
        caller: cx.render(rsx!(UserImageGroup {
            participants: build_participants(&participants),
            with_username: State::join_usernames(&participants),
        },)),
        callee: cx.render(rsx!(UserImage {
            platform: my_identity.platform,
            status: my_identity.status,
            image: my_identity.photo,
            with_username: my_identity.username,
        })),
        description: get_local_text("remote-controls.incoming-call"),
        with_accept_btn: cx.render(rsx!(Button {
            icon: Icon::Phone,
            appearance: Appearance::Success,
            onpress: move |_| {
                ch.send(CallDialogCmd::Accept(call.id));
            }
        })),
        with_deny_btn: cx.render(rsx!(Button {
            icon: Icon::PhoneXMark,
            appearance: Appearance::Danger,
            onpress: move |_| {
                ch.send(CallDialogCmd::Reject(call.id));
            }
        })),
    }))
}

fn get_router(cx: Scope) -> Element {
    let state = use_shared_state::<State>(cx)?;
    let pending_friends = state.read().friends().incoming_requests.len();

    let chat_route = UIRoute {
        to: UPLINK_ROUTES.chat,
        name: get_local_text("uplink.chats"),
        icon: Icon::ChatBubbleBottomCenterText,
        ..UIRoute::default()
    };
    let settings_route = UIRoute {
        to: UPLINK_ROUTES.settings,
        name: get_local_text("settings.settings"),
        icon: Icon::Cog6Tooth,
        ..UIRoute::default()
    };
    let friends_route = UIRoute {
        to: UPLINK_ROUTES.friends,
        name: get_local_text("friends.friends"),
        icon: Icon::Users,
        with_badge: if pending_friends > 0 {
            Some(pending_friends.to_string())
        } else {
            None
        },
        loading: None,
    };
    let files_route = UIRoute {
        to: UPLINK_ROUTES.files,
        name: get_local_text("files.files"),
        icon: Icon::Folder,
        ..UIRoute::default()
    };
    let routes = vec![
        chat_route.clone(),
        files_route.clone(),
        friends_route.clone(),
        settings_route.clone(),
    ];

    let initial_friend_page = use_ref(cx, || FriendRoute::All);

    cx.render(rsx!(
        Router {
            Route {
                to: UPLINK_ROUTES.loading,
                LoadingLayout{}
            },
            Route {
                to: UPLINK_ROUTES.chat,
                ChatLayout {
                    route_info: RouteInfo {
                        routes: routes.clone(),
                        active: chat_route.clone(),
                    }
                }
            },
            Route {
                to: UPLINK_ROUTES.settings,
                SettingsLayout {
                    route_info: RouteInfo {
                        routes: routes.clone(),
                        active: settings_route.clone(),
                    }
                }
            },
            Route {
                to: UPLINK_ROUTES.friends,
                FriendsLayout {
                    route_info: RouteInfo {
                        routes: routes.clone(),
                        active: friends_route.clone(),
                    },
                    initial_page: initial_friend_page.clone()
                }
            },
            Route {
                to: UPLINK_ROUTES.files,
                FilesLayout {
                    route_info: RouteInfo {
                        routes: routes.clone(),
                        active: files_route,
                    }
                }
            },
            notification_action_handler {
                friend_state: initial_friend_page
            }
        }
    ))
}

// handle notification actions
// we need this here as an element to e.g. change routings

#[derive(Props)]
struct NotificationProps<'a> {
    friend_state: &'a UseRef<FriendRoute>,
}

fn notification_action_handler<'a>(cx: Scope<'a, NotificationProps<'a>>) -> Element<'a> {
    let state = use_shared_state::<State>(cx)?;
    let route = use_router(cx);
    let friend_state = cx.props.friend_state;

    use_future(cx, (), |_| {
        to_owned![state, route, friend_state];
        async move {
            let listener_channel = NOTIFICATION_LISTENER.rx.clone();
            log::trace!("starting notification action listener");
            let mut ch = listener_channel.lock().await;
            while let Some(cmd) = ch.recv().await {
                log::debug!("handling notification action {:#?}", cmd);
                match cmd {
                    NotificationAction::DisplayChat(uuid) => {
                        state.write_silent().mutate(Action::ChatWith(&uuid, true));
                        route.replace_route(UPLINK_ROUTES.chat, None, None);
                    }
                    NotificationAction::FriendListPending => {
                        *friend_state.write_silent() = FriendRoute::Pending;
                        route.replace_route(UPLINK_ROUTES.friends, None, None);
                    }
                    NotificationAction::Dummy => {}
                }
            }
        }
    });
    cx.render(rsx!(()))
}
