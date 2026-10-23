mod output;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, ValueEnum};
use qbx_lua_analysis::lint::{lint_paths, FileReport};
use qbx_lua_analysis::{apply_fixes, rules, Config, Level, Severity};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Pretty,
    Compact,
    Json,
    Junit,
    Github,
    Sarif,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum MinSeverity {
    Hint,
    Info,
    Warning,
    Error,
}

impl From<MinSeverity> for Severity {
    fn from(value: MinSeverity) -> Self {
        match value {
            MinSeverity::Hint => Severity::Hint,
            MinSeverity::Info => Severity::Info,
            MinSeverity::Warning => Severity::Warning,
            MinSeverity::Error => Severity::Error,
        }
    }
}

/// A fast linter for FiveM Lua resources.
#[derive(Parser, Debug)]
#[command(name = "qbx-lint", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Files or directories to lint.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Output format.
    #[arg(short, long, value_enum, default_value = "pretty")]
    format: Format,

    /// Write the report to a file instead of stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Path to a qbxlint.toml that applies to every file; by default each file follows those in its
    /// folder and the folders above it.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Apply every available automatic fix, then report what is left.
    #[arg(long)]
    fix: bool,

    /// Lowest severity to report.
    #[arg(long, value_enum, default_value = "info")]
    min_severity: MinSeverity,

    /// Exit with a failure when more than this many warnings are found.
    #[arg(long)]
    max_warnings: Option<usize>,

    /// Override a rule level, e.g. --rule unused-argument=off. May be repeated.
    #[arg(long = "rule", value_name = "CODE=LEVEL")]
    rule_overrides: Vec<String>,

    /// Never fail the process because of lint findings.
    #[arg(long)]
    no_fail: bool,

    /// Disable colored output.
    #[arg(long)]
    no_color: bool,

    /// Print every rule with its default level and exit.
    #[arg(long)]
    list_rules: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Format Lua files in place. Options come from the [format] table of qbxlint.toml.
    Fmt {
        /// Files or directories to format.
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,

        /// Do not write anything; exit with 1 when a file is not formatted.
        #[arg(long)]
        check: bool,

        /// Path to a qbxlint.toml that applies to every file.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

fn run_fmt(paths: &[PathBuf], check: bool, config: Option<&PathBuf>) -> Result<ExitCode, String> {
    use qbx_lua_analysis::project::{lua_files_under, read_source_for_edit};

    let config = match config {
        Some(path) => Config::load(path)?,
        None => discover_config(paths.first().cloned().unwrap_or_else(|| PathBuf::from(".")))?,
    };
    let mut files = Vec::new();
    for path in paths {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.clone());
        if !path.exists() {
            return Err(format!("path does not exist: {}", path.display()));
        }
        if path.is_dir() {
            files.extend(lua_files_under(&path, &config));
        } else if !config.is_excluded(&path) {
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    check_config(&config)?;

    let (mut changed, mut failed) = (0usize, 0usize);
    for file in &files {
        let source = match read_source_for_edit(file) {
            Ok(Some(source)) => source,
            Ok(None) => continue,
            Err(error) => {
                failed += 1;
                eprintln!("{}: {error}; file was not changed", file.display());
                continue;
            }
        };
        match qbx_lua_fmt::format(&source, &config.format_for(file).unwrap_or_default()) {
            Ok(formatted) if formatted == source => {}
            Ok(formatted) => {
                changed += 1;
                println!("{} {}", if check { "would reformat" } else { "reformatted" }, file.display());
                if !check {
                    std::fs::write(file, formatted).map_err(|e| format!("{}: {e}", file.display()))?;
                }
            }
            Err(error) => {
                failed += 1;
                eprintln!("{}: {error}", file.display());
            }
        }
    }
    println!(
        "{} file(s) checked, {changed} {}, {failed} skipped",
        files.len(),
        if check { "need formatting" } else { "reformatted" }
    );
    Ok(if (check && changed > 0) || failed > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(Command::Fmt { paths, check, config }) = &cli.command {
        return run_fmt(paths, *check, config.as_ref()).unwrap_or_else(|message| {
            eprintln!("qbx-lint: {message}");
            ExitCode::from(2)
        });
    }
    if cli.list_rules {
        print_rules();
        return ExitCode::SUCCESS;
    }
    match run(&cli) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("qbx-lint: {message}");
            ExitCode::from(2)
        }
    }
}

fn print_rules() {
    for rule in rules::RULES {
        let level = rule.default.map_or("off", Severity::label);
        let fixable = if rule.fixable { " (fixable)" } else { "" };
        println!("{:<32} {:<8} {:<12} {}{fixable}", rule.code, level, rule.category.label(), rule.summary);
    }
}

fn parse_level(text: &str) -> Option<Level> {
    Some(match text {
        "off" => Level::Off,
        "hint" => Level::Hint,
        "info" => Level::Info,
        "warn" | "warning" => Level::Warning,
        "error" => Level::Error,
        _ => return None,
    })
}

fn load_config(cli: &Cli) -> Result<Config, String> {
    if let Some(path) = &cli.config {
        return Config::load(path);
    }
    discover_config(cli.paths.first().cloned().unwrap_or_else(|| PathBuf::from(".")))
}

/// The configuration files of each path's folder and those above it. Those above `start` are read
/// first, so that one that cannot be read fails the run before anything is analyzed.
fn discover_config(start: PathBuf) -> Result<Config, String> {
    let start = std::path::absolute(&start).unwrap_or(start);
    let config = Config::discover(&start);
    fail_on_unreadable(&config)?;
    Ok(config)
}

/// Prints which LuaLS or EmmyLua files discovery fell back to, and fails when a configuration file
/// it found later, such as one in a folder below the first path, could not be read.
fn check_config(config: &Config) -> Result<(), String> {
    for note in config.notes() {
        eprintln!("qbx-lint: {note}");
    }
    fail_on_unreadable(config)
}

fn fail_on_unreadable(config: &Config) -> Result<(), String> {
    let errors = config.errors();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn run(cli: &Cli) -> Result<ExitCode, String> {
    for path in &cli.paths {
        if !path.exists() {
            return Err(format!("path does not exist: {}", path.display()));
        }
    }
    let mut config = load_config(cli)?;
    for entry in &cli.rule_overrides {
        let (code, level) = entry.split_once('=').ok_or_else(|| format!("expected CODE=LEVEL, got '{entry}'"))?;
        let rule = rules::find(code).ok_or_else(|| format!("unknown rule '{code}'"))?;
        let level = parse_level(level).ok_or_else(|| format!("unknown level '{level}'"))?;
        config.set_rule(rule.code, level);
    }

    let started = Instant::now();
    let mut reports = lint_paths(&cli.paths, &config);
    check_config(&config)?;
    let mut fixed = 0usize;
    if cli.fix {
        for report in &reports {
            let Some(source) = qbx_lua_analysis::project::read_source_for_edit(&report.path)
                .map_err(|e| format!("{}: {e}; file was not changed", report.path.display()))?
            else {
                continue;
            };
            if source != report.source {
                return Err(format!("{}: source changed during analysis; file was not changed", report.path.display()));
            }
            let (new_source, applied) = apply_fixes(&report.source, &report.diagnostics);
            if applied > 0 {
                if !qbx_lua_syntax::parse(&new_source).errors.is_empty() {
                    return Err(format!(
                        "{}: automatic fixes would produce invalid Lua; file was not changed",
                        report.path.display()
                    ));
                }
                std::fs::write(&report.path, new_source).map_err(|e| format!("{}: {e}", report.path.display()))?;
                fixed += applied;
            }
        }
        if fixed > 0 {
            reports = lint_paths(&cli.paths, &config);
        }
    }

    let min: Severity = cli.min_severity.into();
    for report in &mut reports {
        report.diagnostics.retain(|d| d.severity >= min);
    }
    let elapsed = started.elapsed();

    let count = |reports: &[FileReport], severity: Severity| {
        reports.iter().flat_map(|r| &r.diagnostics).filter(|d| d.severity == severity).count()
    };
    let errors = count(&reports, Severity::Error);
    let warnings = count(&reports, Severity::Warning);

    let color = !cli.no_color
        && cli.output.is_none()
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stdout().is_terminal();
    let rendered = output::render(&reports, cli.format, &output::Options { color, fixed, elapsed });
    match &cli.output {
        Some(path) => std::fs::write(path, rendered).map_err(|e| format!("{}: {e}", path.display()))?,
        None => print!("{rendered}"),
    }

    let failed = errors > 0 || cli.max_warnings.is_some_and(|max| warnings > max);
    Ok(if failed && !cli.no_fail { ExitCode::from(1) } else { ExitCode::SUCCESS })
}
