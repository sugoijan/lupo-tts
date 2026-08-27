use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

const MACOS_TTS_HELPER_BINARY: &str = "lupo-tts-macos-helper";

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Workspace task runner",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Bundle a macOS .app for one of the front-end binaries.
    #[command(name = "bundle")]
    BundleMacos(BundleMacosArgs),
    /// Build a workspace binary and the macOS TTS helper together.
    #[command(name = "dev-build")]
    DevBuild(DevBuildArgs),
    /// Build and run a workspace binary with the macOS TTS helper present in target dir.
    #[command(name = "dev-run")]
    DevRun(DevRunArgs),
    /// Regenerate obs-sys bindings from vendor/obs-studio for the host target.
    #[command(name = "obs-bindgen")]
    ObsBindgen,
    /// Build the OBS plugin and package it as a macOS .plugin bundle.
    #[command(name = "bundle-obs")]
    BundleObs(BundleObsArgs),
    /// Build the Dioxus dock UI to wasm and run wasm-bindgen over it.
    #[command(name = "build-dock-ui")]
    BuildDockUi,
}

#[derive(Args)]
struct BundleObsArgs {
    /// Build with the `obs-release` profile (release + panic=unwind) instead of dev.
    #[arg(long)]
    release: bool,

    /// Skip rebuilding the plugin before bundling.
    #[arg(long)]
    skip_build: bool,

    /// Copy the bundle into the user's OBS plugins directory, replacing any previous copy.
    #[arg(long)]
    install: bool,
}

#[derive(Args)]
struct BundleMacosArgs {
    /// Binary name to bundle (e.g. lupo).
    #[arg(long, default_value = "lupo")]
    app: String,

    /// Override the display name shown in Finder.
    #[arg(long)]
    display_name: Option<String>,

    /// Skip rebuilding the binary before bundling.
    #[arg(long)]
    skip_build: bool,

    /// Open the resulting .app bundle after creation.
    #[arg(long)]
    open: bool,
}

#[derive(Args)]
struct DevBuildArgs {
    /// Binary name to build (e.g. lupo, lupo-cli).
    #[arg(long, default_value = "lupo")]
    bin: String,

    /// Build with release profile.
    #[arg(long)]
    release: bool,
}

#[derive(Args)]
struct DevRunArgs {
    /// Binary name to run (e.g. lupo, lupo-cli).
    #[arg(long, default_value = "lupo")]
    bin: String,

    /// Run with release profile.
    #[arg(long)]
    release: bool,

    /// Arguments forwarded to the target binary.
    #[arg(last = true)]
    args: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::BundleMacos(args) => bundle_macos(args),
        Commands::DevBuild(args) => dev_build(args),
        Commands::DevRun(args) => dev_run(args),
        Commands::ObsBindgen => obs_bindgen(),
        Commands::BundleObs(args) => bundle_obs(args),
        Commands::BuildDockUi => build_dock_ui().map(|_| ()),
    }
}

fn repo_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask manifest dir has no parent")?
        .to_path_buf())
}

/// Target triple of the machine running xtask; bindings are committed per triple.
fn host_triple() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        (os, arch) => anyhow::bail!("unsupported host {os}/{arch} for obs-bindgen"),
    }
}

const OBS_PLUGIN_MODULE: &str = "lupo";
const OBS_PLUGIN_PACKAGE: &str = "lupo-obs";
const OBS_PLUGIN_DYLIB: &str = "liblupo_obs.dylib";

fn obs_bindgen() -> Result<()> {
    let repo_root = repo_root()?;
    let libobs_dir = repo_root.join("vendor/obs-studio/libobs");
    if !libobs_dir.join("obs-module.h").exists() {
        anyhow::bail!(
            "{} is missing; run `git submodule update --init vendor/obs-studio`",
            libobs_dir.display()
        );
    }
    let triple = host_triple()?;
    let wrapper = repo_root.join("obs-sys/wrapper.h");
    let out_path = repo_root
        .join("obs-sys/src/bindings")
        .join(format!("{triple}.rs"));

    let mut builder = bindgen::Builder::default()
        .header(wrapper.to_string_lossy())
        .clang_arg(format!("-I{}", libobs_dir.display()))
        .clang_arg(format!("-I{}", repo_root.join("vendor/obs-studio/UI/obs-frontend-api").display()))
        .clang_arg(format!("--target={triple}"))
        .rust_target(bindgen::RustTarget::stable(85, 0).map_err(|e| anyhow::anyhow!("{e}"))?)
        .rust_edition(bindgen::RustEdition::Edition2024)
        // libobs API surface.
        .allowlist_function("obs_.*|blog|blogva|bmalloc|bzalloc|bfree|bstrdup|os_get_config_path.*|os_gettime_ns|os_sleepto_ns|os_sleep_ms|signal_handler_.*|calldata_.*|obs_frontend_.*")
        .allowlist_type("obs_.*|audio_.*|video_.*|speaker_layout|media_frames_per_second|gs_.*|vec2|vec3|vec4")
        .allowlist_var("OBS_.*|LIBOBS_.*|LOG_.*|MAX_AUDIO_.*|MAX_AV_PLANES|AUDIO_OUTPUT_.*")
        // Exported *by* the module, not by libobs; the plugin crate defines them.
        .blocklist_function(
            "obs_module_(load|unload|post_load|ver|set_pointer|name|description|set_locale|free_locale|get_string)|obs_current_module",
        )
        .prepend_enum_name(false)
        .derive_default(true)
        .derive_debug(true)
        .generate_comments(false)
        .layout_tests(true)
        .sort_semantically(true)
        .merge_extern_blocks(true);

    if cfg!(target_os = "macos") {
        let sdk = Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
            .context("xcrun --show-sdk-path failed; install Xcode command line tools")?;
        let sdk = String::from_utf8_lossy(&sdk.stdout).trim().to_string();
        builder = builder.clang_arg(format!("-isysroot{sdk}"));
    }

    let bindings = builder
        .generate()
        .map_err(|e| anyhow::anyhow!("bindgen failed: {e}"))?;

    fs::create_dir_all(out_path.parent().unwrap())?;
    let mut file = fs::File::create(&out_path)
        .with_context(|| format!("failed to create {}", out_path.display()))?;
    writeln!(
        file,
        "// SPDX-License-Identifier: Apache-2.0\n// Generated by `cargo xtask obs-bindgen` from vendor/obs-studio for {triple}. Do not edit."
    )?;
    bindings
        .write(Box::new(file))
        .context("failed to write bindings")?;
    println!("Wrote {}", out_path.display());
    Ok(())
}

/// Compile `lupo-dock-ui` for wasm and emit the wasm-bindgen output (plus
/// `index.html`) into `target/dock-ui/`. Returns that directory.
fn build_dock_ui() -> Result<PathBuf> {
    let repo_root = repo_root()?;
    let mut build_cmd = Command::new("cargo");
    build_cmd.args([
        "build",
        "-p",
        "lupo-dock-ui",
        "--target",
        "wasm32-unknown-unknown",
        "--release",
    ]);
    run(build_cmd.current_dir(&repo_root)).context("failed to build the dock UI for wasm")?;

    let wasm = repo_root.join("target/wasm32-unknown-unknown/release/lupo-dock-ui.wasm");
    if !wasm.exists() {
        anyhow::bail!("wasm artifact not found at {}", wasm.display());
    }
    let out_dir = repo_root.join("target/dock-ui");
    let _ = fs::remove_dir_all(&out_dir);
    fs::create_dir_all(&out_dir)?;

    let mut bindgen = wasm_bindgen_cli_support::Bindgen::new();
    bindgen
        .input_path(&wasm)
        .web(true)
        .map_err(|err| anyhow::anyhow!("wasm-bindgen: {err}"))?
        .out_name("dock_ui")
        .typescript(false)
        .remove_name_section(true)
        .remove_producers_section(true);
    bindgen
        .generate(&out_dir)
        .map_err(|err| anyhow::anyhow!("wasm-bindgen: {err}"))?;
    fs::copy(
        repo_root.join("dock-ui/index.html"),
        out_dir.join("index.html"),
    )?;
    println!("Dock UI written to {}", out_dir.display());
    Ok(out_dir)
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn bundle_obs(args: BundleObsArgs) -> Result<()> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("bundle-obs currently only produces macOS .plugin bundles");
    }
    let repo_root = repo_root()?;
    let profile_dir = if args.release { "obs-release" } else { "debug" };

    if !args.skip_build {
        let mut build_cmd = Command::new("cargo");
        build_cmd.args(["build", "-p", OBS_PLUGIN_PACKAGE]);
        if args.release {
            build_cmd.args(["--profile", "obs-release"]);
        }
        run(build_cmd.current_dir(&repo_root)).context("failed to build the OBS plugin")?;

        let mut helper_cmd = Command::new("cargo");
        helper_cmd.args([
            "build",
            "-p",
            "lupo-tts-macos",
            "--bin",
            MACOS_TTS_HELPER_BINARY,
        ]);
        if args.release {
            helper_cmd.args(["--profile", "obs-release"]);
        }
        run(helper_cmd.current_dir(&repo_root)).context("failed to build the macOS TTS helper")?;
    }

    let ui_dir = build_dock_ui()?;

    let target_dir = repo_root.join("target").join(profile_dir);
    let dylib = target_dir.join(OBS_PLUGIN_DYLIB);
    if !dylib.exists() {
        anyhow::bail!("plugin dylib not found at {}", dylib.display());
    }
    let helper = target_dir.join(MACOS_TTS_HELPER_BINARY);
    if !helper.exists() {
        anyhow::bail!("helper binary not found at {}", helper.display());
    }

    let version = workspace_version(&repo_root).unwrap_or_else(|_| "0.0.0".to_string());
    // OBS resolves `<dir>/<name>.plugin/Contents/MacOS/<name>`, so the executable
    // must carry the bundle's base name.
    let bundle_dir = target_dir
        .join("bundle/obs")
        .join(format!("{OBS_PLUGIN_MODULE}.plugin"));
    let contents_dir = bundle_dir.join("Contents");
    let macos_dir = contents_dir.join("MacOS");
    let resources_dir = contents_dir.join("Resources");
    if bundle_dir.exists() {
        fs::remove_dir_all(&bundle_dir).context("failed to remove existing plugin bundle")?;
    }
    fs::create_dir_all(&macos_dir)?;
    fs::create_dir_all(&resources_dir)?;
    let module_path = macos_dir.join(OBS_PLUGIN_MODULE);
    fs::copy(&dylib, &module_path)
        .with_context(|| format!("failed to copy {}", dylib.display()))?;
    // The plugin looks for the system-voice helper next to its own binary.
    let helper_path = macos_dir.join(MACOS_TTS_HELPER_BINARY);
    fs::copy(&helper, &helper_path)
        .with_context(|| format!("failed to copy {}", helper.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&module_path, &helper_path] {
            let mut perms = fs::metadata(path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms)?;
        }
    }

    // The dock UI is served from the module data path (Contents/Resources).
    copy_dir_recursive(&ui_dir, &resources_dir.join("dock-ui"))?;

    let mut plist = fs::File::create(contents_dir.join("Info.plist"))?;
    write_plist(
        &mut plist,
        &PlistSpec {
            bundle_name: OBS_PLUGIN_MODULE,
            display_name: "LUPO TTS",
            binary_name: OBS_PLUGIN_MODULE,
            identifier: "dev.sugoijan.lupo.obs",
            version: &version,
            package_type: "BNDL",
            min_system_version: "11.0",
            extra_keys: "  <key>CFBundleInfoDictionaryVersion</key>\n  \
                         <string>6.0</string>\n  \
                         <key>CFBundleSupportedPlatforms</key>\n  \
                         <array>\n    <string>MacOSX</string>\n  </array>\n",
        },
    )?;

    // OBS.app disables library validation, so an ad-hoc signature is enough for
    // a local build to load.
    let mut sign = Command::new("codesign");
    sign.args(["--force", "--sign", "-", "--deep"])
        .arg(&bundle_dir);
    run(&mut sign).context("codesign failed")?;

    println!("Plugin bundle created at {}", bundle_dir.display());

    if args.install {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let plugins_dir =
            PathBuf::from(home).join("Library/Application Support/obs-studio/plugins");
        fs::create_dir_all(&plugins_dir)?;
        let dest = plugins_dir.join(format!("{OBS_PLUGIN_MODULE}.plugin"));
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .with_context(|| format!("failed to remove {}", dest.display()))?;
        }
        copy_dir_recursive(&bundle_dir, &dest)
            .context("failed to copy bundle into the OBS plugins directory")?;
        println!("Installed to {}", dest.display());
    }
    Ok(())
}

fn dev_build(args: DevBuildArgs) -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask manifest dir has no parent")?
        .to_path_buf();
    build_binary_with_helper(&repo_root, &args.bin, args.release)
}

fn dev_run(args: DevRunArgs) -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask manifest dir has no parent")?
        .to_path_buf();
    build_binary_with_helper(&repo_root, &args.bin, args.release)?;

    let profile = if args.release { "release" } else { "debug" };
    let binary_path = repo_root.join("target").join(profile).join(&args.bin);
    if !binary_path.exists() {
        anyhow::bail!("binary {} not found at {}", args.bin, binary_path.display());
    }

    let mut cmd = Command::new(&binary_path);
    cmd.args(&args.args);
    run(&mut cmd)
}

fn bundle_macos(args: BundleMacosArgs) -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask manifest dir has no parent")?
        .to_path_buf();

    let binary_name = args.app;
    let display_name = args
        .display_name
        .clone()
        .unwrap_or_else(|| default_display_name(&binary_name));
    let bundle_identifier = bundle_identifier_for(&binary_name);

    if !args.skip_build {
        let mut build_cmd = Command::new("cargo");
        build_cmd.args(["build", "--release", "--bin", &binary_name]);
        if let Some(package_name) = package_for_binary(&binary_name) {
            build_cmd.args(["-p", package_name]);
        }

        run(build_cmd.current_dir(&repo_root))
            .with_context(|| format!("failed to build {binary_name}"))?;

        let mut helper_build_cmd = Command::new("cargo");
        helper_build_cmd.args([
            "build",
            "--release",
            "-p",
            "lupo-tts-macos",
            "--bin",
            MACOS_TTS_HELPER_BINARY,
        ]);
        run(helper_build_cmd.current_dir(&repo_root))
            .context("failed to build macOS TTS helper binary")?;
    }

    let target_dir = repo_root.join("target/release");
    let release_binary = target_dir.join(&binary_name);
    if !release_binary.exists() {
        anyhow::bail!(
            "release binary {} not found at {}",
            binary_name,
            release_binary.display()
        );
    }
    let helper_binary = target_dir.join(MACOS_TTS_HELPER_BINARY);
    if !helper_binary.exists() {
        anyhow::bail!(
            "helper binary {} not found at {}",
            MACOS_TTS_HELPER_BINARY,
            helper_binary.display()
        );
    }

    let version = workspace_version(&repo_root).unwrap_or_else(|_| "0.0.0".to_string());
    let icon_path = repo_root.join("art/Icon-Lupo-TTS.icns");
    let icon_exists = icon_path.exists();

    let sanitized_bundle_name = sanitize_bundle_name(&display_name);
    let bundle_dir = target_dir
        .join("bundle/macos")
        .join(format!("{sanitized_bundle_name}.app"));
    let contents_dir = bundle_dir.join("Contents");
    let macos_dir = contents_dir.join("MacOS");
    let helpers_dir = contents_dir.join("Helpers");
    let resources_dir = contents_dir.join("Resources");

    if bundle_dir.exists() {
        fs::remove_dir_all(&bundle_dir).context("failed to remove existing bundle directory")?;
    }
    fs::create_dir_all(&macos_dir).context("failed to create MacOS directory")?;
    fs::create_dir_all(&helpers_dir).context("failed to create Helpers directory")?;
    fs::create_dir_all(&resources_dir).context("failed to create Resources directory")?;

    fs::copy(&release_binary, macos_dir.join(&binary_name))
        .with_context(|| format!("failed to copy binary {}", release_binary.display()))?;
    fs::copy(&helper_binary, helpers_dir.join(MACOS_TTS_HELPER_BINARY))
        .with_context(|| format!("failed to copy helper binary {}", helper_binary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(macos_dir.join(&binary_name))?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(macos_dir.join(&binary_name), perms)?;
        let mut helper_perms =
            fs::metadata(helpers_dir.join(MACOS_TTS_HELPER_BINARY))?.permissions();
        helper_perms.set_mode(0o755);
        fs::set_permissions(helpers_dir.join(MACOS_TTS_HELPER_BINARY), helper_perms)?;
    }

    if icon_exists {
        fs::copy(&icon_path, resources_dir.join("Icon.icns"))
            .with_context(|| format!("failed to copy icon {}", icon_path.display()))?;
    }

    let mut plist = fs::File::create(contents_dir.join("Info.plist"))?;
    let mut extra_keys = String::from(
        "  <key>NSHighResolutionCapable</key>\n  \
         <true/>\n  \
         <key>LSApplicationCategoryType</key>\n  \
         <string>public.app-category.utilities</string>\n",
    );
    if icon_exists {
        extra_keys.push_str("  <key>CFBundleIconFile</key>\n  <string>Icon</string>\n");
    }
    write_plist(
        &mut plist,
        &PlistSpec {
            bundle_name: &display_name,
            display_name: &display_name,
            binary_name: &binary_name,
            identifier: &bundle_identifier,
            version: &version,
            package_type: "APPL",
            min_system_version: "10.15",
            extra_keys: &extra_keys,
        },
    )?;

    println!("Bundle created at {}", bundle_dir.display());
    println!(
        "Optional: codesign -s - --deep --force \"{}\"",
        bundle_dir.display()
    );

    if args.open {
        open_bundle(&bundle_dir)?;
    }

    Ok(())
}
fn open_bundle(bundle_dir: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("open");
        cmd.arg(bundle_dir);
        run(&mut cmd)?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        println!("--open flag ignored: opening bundles only supported on macOS");
    }
    Ok(())
}

fn workspace_version(repo_root: &Path) -> Result<String> {
    let cargo_toml = fs::read_to_string(repo_root.join("Cargo.toml"))
        .context("failed to read workspace Cargo.toml")?;
    for line in cargo_toml.lines() {
        if let Some(rest) = line.trim().strip_prefix("version")
            && let Some(eq_index) = rest.find('=')
        {
            let value = rest[eq_index + 1..].trim();
            if let Some(stripped) = value.trim_matches('"').split('#').next()
                && !stripped.is_empty()
            {
                return Ok(stripped.to_string());
            }
        }
    }
    anyhow::bail!("version not found in workspace Cargo.toml");
}

fn sanitize_bundle_name(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                ' '
            }
        })
        .collect();
    sanitized = sanitized.trim().to_string();
    if sanitized.is_empty() {
        "Lupo".to_string()
    } else {
        sanitized
    }
}

fn default_display_name(binary: &str) -> String {
    match binary {
        "lupo" => "Lupo".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Lupo".to_string(),
            }
        }
    }
}

fn package_for_binary(binary: &str) -> Option<&'static str> {
    match binary {
        "lupo" => Some("lupo-app"),
        "lupo-cli" => Some("lupo-cli"),
        _ => None,
    }
}

fn bundle_identifier_for(binary: &str) -> String {
    match binary {
        "lupo" => "dev.sugoijan.lupo".to_string(),
        other => format!("dev.sugoijan.lupo.{other}"),
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {:?}", cmd))?;
    if !status.success() {
        anyhow::bail!("command {:?} exited with {}", cmd, status);
    }
    Ok(())
}

fn build_binary_with_helper(repo_root: &Path, binary_name: &str, release: bool) -> Result<()> {
    let mut build_cmd = Command::new("cargo");
    build_cmd.arg("build");
    if release {
        build_cmd.arg("--release");
    }
    build_cmd.args(["--bin", binary_name]);
    if let Some(package_name) = package_for_binary(binary_name) {
        build_cmd.args(["-p", package_name]);
    }
    run(build_cmd.current_dir(repo_root))
        .with_context(|| format!("failed to build {binary_name}"))?;

    if cfg!(target_os = "macos") {
        let mut helper_build_cmd = Command::new("cargo");
        helper_build_cmd.arg("build");
        if release {
            helper_build_cmd.arg("--release");
        }
        helper_build_cmd.args(["-p", "lupo-tts-macos", "--bin", MACOS_TTS_HELPER_BINARY]);
        run(helper_build_cmd.current_dir(repo_root))
            .context("failed to build macOS TTS helper binary")?;
    }

    Ok(())
}

struct PlistSpec<'a> {
    bundle_name: &'a str,
    display_name: &'a str,
    binary_name: &'a str,
    identifier: &'a str,
    version: &'a str,
    /// "APPL" for apps, "BNDL" for plugins.
    package_type: &'a str,
    min_system_version: &'a str,
    /// Extra pre-formatted (and pre-escaped) XML lines inside the dict.
    extra_keys: &'a str,
}

fn write_plist(file: &mut fs::File, spec: &PlistSpec) -> Result<()> {
    writeln!(
        file,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>{name}</string>
  <key>CFBundleDisplayName</key>
  <string>{display}</string>
  <key>CFBundleExecutable</key>
  <string>{binary}</string>
  <key>CFBundleIdentifier</key>
  <string>{identifier}</string>
  <key>CFBundleVersion</key>
  <string>{version}</string>
  <key>CFBundleShortVersionString</key>
  <string>{version}</string>
  <key>CFBundlePackageType</key>
  <string>{package_type}</string>
  <key>LSMinimumSystemVersion</key>
  <string>{min_system}</string>
{extra}</dict>
</plist>"#,
        name = xml_escape(spec.bundle_name),
        display = xml_escape(spec.display_name),
        binary = xml_escape(spec.binary_name),
        identifier = xml_escape(spec.identifier),
        version = xml_escape(spec.version),
        package_type = spec.package_type,
        min_system = spec.min_system_version,
        extra = spec.extra_keys,
    )?;
    Ok(())
}

fn xml_escape(input: &str) -> String {
    input
        .chars()
        .flat_map(|c| match c {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect::<Vec<_>>(),
            '>' => "&gt;".chars().collect::<Vec<_>>(),
            '"' => "&quot;".chars().collect::<Vec<_>>(),
            '\'' => "&apos;".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect()
}
