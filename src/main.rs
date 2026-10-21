use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use indicatif::HumanBytes;

use jarsuite::assemble;
use jarsuite::install::{Activated, InUse, Root, Update};

/// Installs, updates and launches a suite of Java tools released together.
#[derive(Parser)]
#[command(name = "jarsuite", version, about)]
struct Cli {
    /// Installation folder (default: $JARSUITE_ROOT, then the folder of this executable)
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compose a release from the tools' locks and JARs (at build time).
    Assemble {
        /// The suite definition
        #[arg(default_value = "suite-def.toml")]
        def: PathBuf,
        /// Output folder for the archives and suite.toml
        #[arg(short, long, default_value = "dist")]
        out: PathBuf,
        /// URL the archives will be published under (default: next to suite.toml)
        #[arg(long)]
        base_url: Option<String>,
        /// Local jarexec binary for a platform, e.g. linux-x64=path/to/jarexec (repeatable)
        #[arg(long, value_parser = platform_path)]
        jarexec: Vec<(String, PathBuf)>,
        /// Build only this platform instead of those in the definition (repeatable)
        #[arg(long, value_parser = known_platform)]
        platform: Vec<String>,
    },
    /// Follow a published suite: URL or path of its suite.toml.
    Init { manifest: String },
    /// Show the installed releases; with --check also the published one.
    Status {
        #[arg(long)]
        check: bool,
    },
    /// Download and prepare the published release, then activate it if no tool is running.
    Update {
        /// Only prepare it: `activate` switches later
        #[arg(long)]
        no_activate: bool,
    },
    /// Switch to an installed release (default: the one prepared by `update`).
    Activate {
        release: Option<String>,
        /// Allow a release with a lower data format than the data may be in
        #[arg(long)]
        force: bool,
    },
    /// Go back to the previous release.
    Rollback {
        /// Allow a release with a lower data format than the data may be in
        #[arg(long)]
        force: bool,
    },
    /// Launch a tool of the active release; the remaining arguments go to the tool.
    Run {
        tool: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Remove old releases and what the cache holds only for them.
    Clean,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("jarsuite: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    if let Command::Assemble {
        def,
        out,
        base_url,
        jarexec,
        platform,
    } = cli.command
    {
        let done = assemble::run(&assemble::Options {
            def,
            out,
            base_url,
            jarexec,
            platforms: platform,
        })?;
        for warning in &done.warnings {
            eprintln!("warning: {warning}");
        }
        for archive in &done.archives {
            println!("wrote {}", archive.display());
        }
        println!("wrote {}", done.manifest.display());
        return Ok(ExitCode::SUCCESS);
    }

    let root = Root::new(jarsuite::install::root_dir(cli.root)?);
    match cli.command {
        Command::Assemble { .. } => unreachable!("handled above"),
        Command::Init { manifest } => {
            let url = root.init(&manifest)?;
            println!("{} follows {url}", root.dir().display());
        }
        Command::Status { check } => status(&root, check)?,
        Command::Update { no_activate } => {
            let cache = root.cache();
            let outcome = root.update(
                |lock| jarexec::launch::prepare(lock, &cache, None, false).map(|_| ()),
                |step| eprintln!("{step}"),
            )?;
            match outcome {
                Update::UpToDate(r) => println!("release {r} is already active"),
                Update::Ready(r) if no_activate => {
                    println!("release {r} is ready: `jarsuite activate` switches to it")
                }
                Update::Ready(r) => match root.activate(None, false) {
                    Ok(done) => report(&done),
                    Err(e) if e.is::<InUse>() => {
                        println!(
                            "release {r} is ready, but {e:#}: `jarsuite activate` switches to it"
                        );
                        return Ok(ExitCode::from(2));
                    }
                    Err(e) => return Err(e),
                },
            }
        }
        Command::Activate { release, force } => report(&root.activate(release.as_deref(), force)?),
        Command::Rollback { force } => report(&root.rollback(force)?),
        Command::Run { tool, args } => {
            // Ctrl+C reaches the tool too: let it decide how to exit, and wait for it
            let _ = ctrlc::set_handler(|| {});
            let code = jarsuite::run::run(&root, &tool, &args)?;
            std::process::exit(code);
        }
        Command::Clean => {
            let (removed, cache) = root.clean()?;
            for r in removed {
                println!("removed release {r}");
            }
            print_cache(cache.as_ref());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn status(root: &Root, check: bool) -> Result<()> {
    let state = root.state()?;
    let show = |r: &Option<String>| r.clone().unwrap_or_else(|| "-".into());
    println!("root:      {}", root.dir().display());
    println!("manifest:  {}", show(&state.url));
    println!("current:   {}", show(&state.current));
    println!("previous:  {}", show(&state.previous));
    println!("pending:   {}", show(&state.pending));
    if let Some(format) = state.data_format {
        println!("data:      format {format}");
    }
    if let Some(current) = &state.current {
        println!(
            "tools:     {}",
            root.release_manifest(current)?.tools.join(", ")
        );
    }
    println!("installed: {}", root.installed()?.join(", "));
    if check {
        let (_, manifest) = root.fetch_manifest()?;
        let published = manifest.suite.release;
        let note = if state.current.as_deref() == Some(&published) {
            "active"
        } else {
            "run `jarsuite update`"
        };
        println!("published: {published} ({note})");
    }
    Ok(())
}

fn report(done: &Activated) {
    println!("release {} is active", done.release);
    for r in &done.removed {
        println!("removed release {r}");
    }
    print_cache(done.cache.as_ref());
}

fn print_cache(cache: Option<&jarexec::clean::Report>) {
    match cache {
        Some(r) if r.files > 0 => println!(
            "removed {} unused files ({}) from the cache",
            r.files,
            HumanBytes(r.bytes)
        ),
        Some(_) => {}
        None => println!("cache cleanup skipped: a tool is being prepared"),
    }
}

/// `linux-x64=path/to/jarexec`
fn platform_path(arg: &str) -> Result<(String, PathBuf), String> {
    let (platform, path) = arg
        .split_once('=')
        .ok_or_else(|| format!("expected PLATFORM=PATH, got {arg}"))?;
    Ok((known_platform(platform)?, PathBuf::from(path)))
}

fn known_platform(platform: &str) -> Result<String, String> {
    if jarsuite::manifest::PLATFORMS.contains(&platform) {
        Ok(platform.to_string())
    } else {
        Err(format!(
            "unknown platform {platform} (known: {})",
            jarsuite::manifest::PLATFORMS.join(", ")
        ))
    }
}
