// SPDX-License-Identifier: Apache-2.0

#[cfg(feature = "renderer-webview")]
use super::action::{
    MENU_ITEM_OPEN_MANUAL, MENU_ITEM_OPEN_SETTINGS, MENU_ITEM_OPEN_SHORTCUTS,
    MENU_ITEM_TOGGLE_HELP_MODE,
};
#[cfg(any(feature = "renderer-webview", feature = "renderer-native"))]
use super::ui::AppRoot;
#[cfg(all(
    feature = "renderer-webview",
    any(target_os = "windows", target_os = "linux", target_os = "macos")
))]
use dioxus_desktop::muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};
#[cfg(feature = "renderer-webview")]
use dioxus_desktop::{Config as WebviewConfig, launch::launch as launch_webview};
#[cfg(feature = "renderer-native")]
use dioxus_native::launch as launch_native;

pub trait RendererAdapter {
    fn run();
}

pub fn run() {
    #[cfg(feature = "renderer-webview")]
    {
        WebviewRenderer::run();
    }

    #[cfg(feature = "renderer-native")]
    {
        NativeRenderer::run();
    }
}

#[cfg(feature = "renderer-webview")]
pub struct WebviewRenderer;

#[cfg(feature = "renderer-webview")]
impl RendererAdapter for WebviewRenderer {
    fn run() {
        let config = webview_config();
        launch_webview(AppRoot, Vec::new(), vec![Box::new(config)]);
    }
}

#[cfg(feature = "renderer-webview")]
fn webview_config() -> WebviewConfig {
    let config = WebviewConfig::new();

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    {
        config.with_menu(build_menu())
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        config
    }
}

#[cfg(all(
    feature = "renderer-webview",
    any(target_os = "windows", target_os = "linux", target_os = "macos")
))]
fn build_menu() -> Menu {
    let menu = Menu::new();

    let app_menu = Submenu::new("Lupo", true);
    app_menu
        .append_items(&[
            &MenuItem::with_id(MENU_ITEM_OPEN_SETTINGS, "Settings...", true, None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ])
        .expect("failed to append app menu items");

    let window_menu = Submenu::new("Window", true);
    window_menu
        .append_items(&[
            &PredefinedMenuItem::fullscreen(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::maximize(None),
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::close_window(None),
        ])
        .expect("failed to append window menu items");

    let edit_menu = Submenu::new("Edit", true);
    edit_menu
        .append_items(&[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::select_all(None),
        ])
        .expect("failed to append edit menu items");

    let help_menu = Submenu::new("Help", true);
    help_menu
        .append_items(&[
            &MenuItem::with_id(MENU_ITEM_TOGGLE_HELP_MODE, "Toggle Help Mode", true, None),
            &MenuItem::with_id(MENU_ITEM_OPEN_SHORTCUTS, "Shortcut Catalog", true, None),
            &MenuItem::with_id(MENU_ITEM_OPEN_MANUAL, "Open Manual", true, None),
        ])
        .expect("failed to append help menu items");

    menu.append_items(&[&app_menu, &window_menu, &edit_menu, &help_menu])
        .expect("failed to append root menu items");

    #[cfg(target_os = "macos")]
    {
        window_menu.set_as_windows_menu_for_nsapp();
    }

    menu
}

#[cfg(feature = "renderer-native")]
pub struct NativeRenderer;

#[cfg(feature = "renderer-native")]
impl RendererAdapter for NativeRenderer {
    fn run() {
        launch_native(AppRoot);
    }
}
