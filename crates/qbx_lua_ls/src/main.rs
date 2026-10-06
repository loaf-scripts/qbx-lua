use qbx_lua_ls::check::{check, CheckOptions};
use qbx_lua_ls::features::diagnostics::parse_level;

/// `qbx-lua-ls --index <dir>` indexes a folder once and reports what it found, for benchmarking.
fn print_index_stats(dir: std::path::PathBuf) {
    let mut workspace = qbx_lua_ls::workspace::Workspace::default();
    workspace.roots = vec![std::path::absolute(&dir).unwrap_or(dir)];
    workspace.load_stubs();
    let stats = workspace.scan();
    let (mut globals, mut members, mut classes, mut events) = (0, 0, 0, 0);
    for (_, file) in workspace.index.files() {
        globals += file.index.globals.len();
        members += file.index.members.len();
        classes += file.index.classes.len() + file.index.aliases.len();
        events += file.index.events.len();
    }
    println!(
        "{} files, {} resources in {} ms: {globals} globals, {members} members, {classes} types, {events} events",
        stats.files, stats.resources, stats.millis
    );
}

/// The options of `qbx-lua-ls --check`, from the arguments after it.
fn check_options(args: &[String]) -> Result<CheckOptions, String> {
    let mut options = CheckOptions::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strict" => options.settings.strict = Some(true),
            "--library" => options.library.push(args.next().ok_or("--library needs a folder")?.into()),
            "--rule" => {
                let entry = args.next().ok_or("--rule needs CODE=LEVEL")?;
                let (code, level) =
                    entry.split_once('=').ok_or_else(|| format!("expected CODE=LEVEL, got '{entry}'"))?;
                let rule = qbx_lua_analysis::rules::find(code).ok_or_else(|| format!("unknown rule '{code}'"))?;
                let level = parse_level(level).ok_or_else(|| format!("unknown level '{level}'"))?;
                options.settings.levels.push((rule.code.to_string(), level));
            }
            "--max-warnings" => {
                let count = args.next().and_then(|count| count.parse().ok());
                options.max_warnings = count.ok_or("--max-warnings needs a number")?;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option '{flag}'")),
            root => options.roots.push(root.into()),
        }
    }
    if options.roots.is_empty() {
        options.roots.push(".".into());
    }
    if let Some(root) = options.roots.iter().chain(&options.library).find(|root| !root.is_dir()) {
        return Err(format!("not a folder: {}", root.display()));
    }
    Ok(options)
}

/// `qbx-lua-ls --check [<dir>...]` prints what the editor reports for the files of the folders, the
/// type rules included, and fails on an error or on more warnings than `--max-warnings` allows.
fn run_check(args: &[String]) -> std::process::ExitCode {
    let options = match check_options(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("qbx-lua-ls: {message}");
            return std::process::ExitCode::from(2);
        }
    };
    let outcome = check(&options);
    for line in &outcome.lines {
        println!("{line}");
    }
    let count = |count: usize, noun: &str| format!("{count} {noun}{}", if count == 1 { "" } else { "s" });
    let (files, errors) = (count(outcome.files, "file"), count(outcome.errors, "error"));
    eprintln!("{files} checked: {errors}, {}", count(outcome.warnings, "warning"));
    match outcome.failed(&options) {
        true => std::process::ExitCode::from(1),
        false => std::process::ExitCode::SUCCESS,
    }
}

fn main() -> std::process::ExitCode {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("qbx-lua-ls {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "qbx-lua-ls {}\n\nUsage: qbx-lua-ls             start the language server over stdio\n       qbx-lua-ls --check [<dir>...] [--strict] [--library <dir>] [--rule CODE=LEVEL] [--max-warnings <n>]\n                                check the folders as the editor does and print what it reports\n       qbx-lua-ls --index <dir> index a folder once and print what was found\n       qbx-lua-ls --version     print the version",
            env!("CARGO_PKG_VERSION")
        );
        return std::process::ExitCode::SUCCESS;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--check") {
        // Deeply nested Lua is walked recursively; the default main-thread stack is too small for that on Windows.
        let checker = std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(move || run_check(&args[1..]));
        return checker.expect("failed to start the check thread").join().unwrap_or(std::process::ExitCode::from(101));
    }
    if let Some(dir) = std::env::args().skip_while(|arg| arg != "--index").nth(1) {
        print_index_stats(dir.into());
        return std::process::ExitCode::SUCCESS;
    }
    // Deeply nested Lua is walked recursively; the default main-thread stack is too small for that on Windows.
    let server = std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(qbx_lua_ls::server::run);
    let outcome = server.expect("failed to start the server thread").join();
    match outcome {
        Ok(Ok(())) => std::process::ExitCode::SUCCESS,
        Ok(Err(error)) => {
            eprintln!("qbx-lua-ls: {error}");
            std::process::ExitCode::from(1)
        }
        Err(_) => std::process::ExitCode::from(101),
    }
}
