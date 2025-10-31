use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

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
}

#[derive(Args)]
struct BundleMacosArgs {
    /// Binary name to bundle (e.g. lupo or lupo2).
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::BundleMacos(args) => bundle_macos(args),
    }
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
        run(Command::new("cargo")
            .args(["build", "--release", "--bin", &binary_name])
            .current_dir(&repo_root))
        .with_context(|| format!("failed to build {binary_name}"))?;
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

    let version = workspace_version(&repo_root).unwrap_or_else(|_| "0.0.0".to_string());
    let icon_path = repo_root.join("art/Icon-Lupo-TTS.icns");
    let icon_exists = icon_path.exists();

    let sanitized_bundle_name = sanitize_bundle_name(&display_name);
    let bundle_dir = target_dir
        .join("bundle/macos")
        .join(format!("{sanitized_bundle_name}.app"));
    let contents_dir = bundle_dir.join("Contents");
    let macos_dir = contents_dir.join("MacOS");
    let resources_dir = contents_dir.join("Resources");

    if bundle_dir.exists() {
        fs::remove_dir_all(&bundle_dir).context("failed to remove existing bundle directory")?;
    }
    fs::create_dir_all(&macos_dir).context("failed to create MacOS directory")?;
    fs::create_dir_all(&resources_dir).context("failed to create Resources directory")?;

    fs::copy(&release_binary, macos_dir.join(&binary_name))
        .with_context(|| format!("failed to copy binary {}", release_binary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(macos_dir.join(&binary_name))?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(macos_dir.join(&binary_name), perms)?;
    }

    if icon_exists {
        fs::copy(&icon_path, resources_dir.join("Icon.icns"))
            .with_context(|| format!("failed to copy icon {}", icon_path.display()))?;
    }

    let mut plist = fs::File::create(contents_dir.join("Info.plist"))?;
    write_plist(
        &mut plist,
        &display_name,
        &binary_name,
        &bundle_identifier,
        &version,
        icon_exists,
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
        if let Some(rest) = line.trim().strip_prefix("version") {
            if let Some(eq_index) = rest.find('=') {
                let value = rest[eq_index + 1..].trim();
                if let Some(stripped) = value.trim_matches('"').split('#').next() {
                    if !stripped.is_empty() {
                        return Ok(stripped.to_string());
                    }
                }
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
            } else if c.is_whitespace() {
                ' '
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
        "lupo2" => "Lupo (GPUI)".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Lupo".to_string(),
            }
        }
    }
}

fn bundle_identifier_for(binary: &str) -> String {
    match binary {
        "lupo" => "dev.sugoijan.lupo".to_string(),
        "lupo2" => "dev.sugoijan.lupo.gpui".to_string(),
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

fn write_plist(
    file: &mut fs::File,
    display_name: &str,
    binary_name: &str,
    bundle_identifier: &str,
    version: &str,
    has_icon: bool,
) -> Result<()> {
    writeln!(
        file,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>{display}</string>
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
  <string>APPL</string>
  <key>LSMinimumSystemVersion</key>
  <string>10.15</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.utilities</string>
"#,
        display = xml_escape(display_name),
        binary = xml_escape(binary_name),
        identifier = xml_escape(bundle_identifier),
        version = xml_escape(version),
    )?;

    if has_icon {
        writeln!(
            file,
            r#"  <key>CFBundleIconFile</key>
  <string>Icon</string>
"#
        )?;
    }

    writeln!(file, "</dict>\n</plist>")?;
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
