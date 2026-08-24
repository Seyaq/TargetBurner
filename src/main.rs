use std::cmp;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

struct Config {
    root_path: PathBuf,
    days: Option<u64>,
    min_size_bytes: u64,
    dry_run: bool,
    interactive: bool,
    exclude_current: bool,
}

#[derive(Debug)]
struct TargetInfo {
    path: PathBuf,
    size_bytes: u64,
    last_modified: SystemTime,
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Support pour `cargo lucifer` (cargo passe "lucifer" comme premier argument)
    let start_idx = if args.len() > 1 && args[1] == "lucifer" {
        2
    } else {
        1
    };
    let cli_args = &args[start_idx..];

    let config = parse_args(cli_args);

    let current_dir_target = if config.exclude_current {
        env::current_dir()
            .ok()
            .and_then(|p| fs::canonicalize(p.join("target")).ok())
    } else {
        None
    };

    println!("[lucifer] Scanning path: {:?}", config.root_path);
    if config.dry_run {
        println!("[lucifer] DRY-RUN MODE: No files will be deleted.");
    }

    let start_scan = Instant::now();
    let mut raw_targets = Vec::new();
    scan_rust_projects(&config.root_path, &mut raw_targets);
    let scan_duration = start_scan.elapsed();

    if raw_targets.is_empty() {
        println!("[lucifer] No target directories found.");
        println!("[lucifer] Scan completed in {:.2?}", scan_duration);
        return;
    }

    // Parallélisation du calcul des tailles via std::thread::scope
    let start_size = Instant::now();
    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let targets_arc = Arc::new(Mutex::new(raw_targets));
    let results_arc = Arc::new(Mutex::new(Vec::new()));

    thread::scope(|s| {
        for _ in 0..num_threads {
            let targets = Arc::clone(&targets_arc);
            let results = Arc::clone(&results_arc);
            s.spawn(move || {
                loop {
                    let target = {
                        let mut lock = targets.lock().unwrap();
                        lock.pop()
                    };
                    let Some(path) = target else { break };
                    let (size, last_mod) = dir_info(&path);
                    results.lock().unwrap().push(TargetInfo {
                        path,
                        size_bytes: size,
                        last_modified: last_mod,
                    });
                }
            });
        }
    });

    let mut targets = Arc::try_unwrap(results_arc).unwrap().into_inner().unwrap();
    let size_calc_duration = start_size.elapsed();

    // Application des filtres (--exclude-current, --days, --min-size)
    let now = SystemTime::now();
    targets.retain(|t| {
        if let Some(ref protected) = current_dir_target {
            if let Ok(canon) = fs::canonicalize(&t.path) {
                if &canon == protected {
                    return false;
                }
            }
        }

        if t.size_bytes < config.min_size_bytes {
            return false;
        }

        if let Some(days) = config.days {
            if let Ok(elapsed) = now.duration_since(t.last_modified) {
                if elapsed.as_secs() < days * 86400 {
                    return false;
                }
            }
        }

        true
    });

    if targets.is_empty() {
        println!("[lucifer] No target directories matched the filter criteria.");
        return;
    }

    // Sélection interactive ou confirmation standard
    let selected_targets = if config.interactive && is_tty() {
        interactive_select(&targets)
    } else {
        println!("\nTarget directories matching criteria:");
        for t in &targets {
            println!("  - {} ({})", t.path.display(), format_bytes(t.size_bytes));
        }

        let total: u64 = targets.iter().map(|t| t.size_bytes).sum();
        println!("\nReclaimable space: {}", format_bytes(total));
        print!("\nConfirm purge of all listed directories? [y/N]: ");
        let _ = io::stdout().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y") {
            targets
        } else {
            Vec::new()
        }
    };

    if selected_targets.is_empty() {
        println!("[lucifer] Aborted. No directories purged.");
        return;
    }

    let total_bytes: u64 = selected_targets.iter().map(|t| t.size_bytes).sum();
    let start_purge = Instant::now();
    let mut purged_count = 0;

    println!();
    for t in &selected_targets {
        if config.dry_run {
            purged_count += 1;
            println!("[dry-run] Would remove: {}", t.path.display());
        } else {
            match fs::remove_dir_all(&t.path) {
                Ok(_) => {
                    purged_count += 1;
                    println!("[+] Removed: {}", t.path.display());
                }
                Err(err) => eprintln!("[!] Error removing {}: {}", t.path.display(), err),
            }
        }
    }
    let purge_duration = start_purge.elapsed();

    let total_mb = total_bytes as f64 / 1_048_576.0;
    let throughput_mb_s = if purge_duration.as_secs_f64() > 0.0 {
        total_mb / purge_duration.as_secs_f64()
    } else {
        0.0
    };

    print_performance_summary(
        purged_count,
        scan_duration,
        size_calc_duration,
        purge_duration,
        throughput_mb_s,
    );
}

fn parse_args(args: &[String]) -> Config {
    let mut config = Config {
        root_path: PathBuf::from("."),
        days: None,
        min_size_bytes: 0,
        dry_run: false,
        interactive: false,
        exclude_current: true,
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" | "--dry-run" => config.dry_run = true,
            "-i" | "--interactive" => config.interactive = true,
            "--no-exclude-current" => config.exclude_current = false,
            "--days" => {
                if i + 1 < args.len() {
                    config.days = args[i + 1].parse().ok();
                    i += 1;
                }
            }
            "--min-size" => {
                if i + 1 < args.len() {
                    config.min_size_bytes = parse_size(&args[i + 1]);
                    i += 1;
                }
            }
            path if !path.starts_with('-') => {
                config.root_path = PathBuf::from(path);
            }
            _ => {}
        }
        i += 1;
    }
    config
}

fn parse_size(input: &str) -> u64 {
    let trimmed = input.trim();
    let (num_str, unit) = trimmed.span_of(|c: char| c.is_ascii_digit() || c == '.');
    let value: f64 = num_str.parse().unwrap_or(0.0);
    match unit.to_uppercase().as_str() {
        "KB" | "K" => (value * 1024.0) as u64,
        "MB" | "M" => (value * 1024.0 * 1024.0) as u64,
        "GB" | "G" => (value * 1024.0 * 1024.0 * 1024.0) as u64,
        _ => value as u64,
    }
}

trait SpanOf {
    fn span_of<F>(&self, pred: F) -> (&str, &str)
    where
        F: Fn(char) -> bool;
}

impl SpanOf for str {
    fn span_of<F>(&self, pred: F) -> (&str, &str)
    where
        F: Fn(char) -> bool,
    {
        let idx = self.find(|c| !pred(c)).unwrap_or(self.len());
        (&self[..idx], &self[idx..])
    }
}

fn scan_rust_projects(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut has_cargo_toml = false;
    let mut is_workspace_root = false;
    let mut target_dir = None;
    let mut sub_dirs = Vec::new();

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }

        let name = entry.file_name();
        let path = entry.path();

        if file_type.is_dir() {
            if name == "target" {
                target_dir = Some(path);
            } else if !is_hidden_or_reserved(&name) {
                sub_dirs.push(path);
            }
        } else if name == "Cargo.toml" {
            has_cargo_toml = true;
            if let Ok(content) = fs::read_to_string(&path) {
                if content.contains("[workspace]") {
                    is_workspace_root = true;
                }
            }
        }
    }

    if has_cargo_toml {
        if let Some(target) = target_dir {
            found.push(target);
        }
        if is_workspace_root {
            return;
        }
    }

    for sub in sub_dirs {
        scan_rust_projects(&sub, found);
    }
}

#[inline]
fn is_hidden_or_reserved(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

fn dir_info(path: &Path) -> (u64, SystemTime) {
    let Ok(entries) = fs::read_dir(path) else {
        return (0, SystemTime::UNIX_EPOCH);
    };
    let mut total_size = 0u64;
    let mut max_mtime = SystemTime::UNIX_EPOCH;

    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }

        if ft.is_dir() {
            let (sub_size, sub_mtime) = dir_info(&entry.path());
            total_size += sub_size;
            if sub_mtime > max_mtime {
                max_mtime = sub_mtime;
            }
        } else if let Ok(m) = entry.metadata() {
            total_size += m.len();
            if let Ok(mtime) = m.modified() {
                if mtime > max_mtime {
                    max_mtime = mtime;
                }
            }
        }
    }
    (total_size, max_mtime)
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn is_tty() -> bool {
    io::stdin().is_terminal()
}

// Mode TUI interactif sans dépendances externes
fn interactive_select(targets: &[TargetInfo]) -> Vec<TargetInfo> {
    let mut selected = vec![true; targets.len()];
    let mut cursor = 0;

    // Basculement en Mode Raw pour le terminal (Unix)
    #[cfg(unix)]
    let _raw_mode_guard = RawModeGuard::new();

    loop {
        // Redessine l'écran
        print!("\x1B[2J\x1B[H"); // Clear screen & Move cursor home
        println!("=== Lucifer Interactive Target Selector ===");
        println!(
            "Use UP/DOWN arrows to navigate, SPACE to toggle, A to select all, ENTER to confirm.\n"
        );

        let mut current_total = 0u64;
        for (i, t) in targets.iter().enumerate() {
            let pointer = if i == cursor { ">" } else { " " };
            let check = if selected[i] { "[X]" } else { "[ ]" };
            if selected[i] {
                current_total += t.size_bytes;
            }
            println!(
                " {} {} {:<40} ({})",
                pointer,
                check,
                t.path.display(),
                format_bytes(t.size_bytes)
            );
        }

        println!("\nSelected space to free: {}", format_bytes(current_total));
        let _ = io::stdout().flush();

        // Lecture des événements clavier
        let mut buf = [0u8; 3];
        let n = io::stdin().read(&mut buf).unwrap_or(0);

        if n == 1 {
            match buf[0] {
                b' ' => selected[cursor] = !selected[cursor],
                b'a' | b'A' => {
                    let all_selected = selected.iter().all(|&b| b);
                    selected.fill(!all_selected);
                }
                13 | 10 => break,       // Key Enter
                3 => return Vec::new(), // Ctrl+C
                _ => {}
            }
        } else if n == 3 && buf[0] == 27 && buf[1] == 91 {
            match buf[2] {
                65 => {
                    if cursor > 0 {
                        cursor -= 1
                    }
                } // Arrow Up
                66 => {
                    if cursor + 1 < targets.len() {
                        cursor += 1
                    }
                } // Arrow Down
                _ => {}
            }
        }
    }

    targets
        .iter()
        .zip(selected)
        .filter_map(|(t, sel)| {
            if sel {
                Some(TargetInfo {
                    path: t.path.clone(),
                    size_bytes: t.size_bytes,
                    last_modified: t.last_modified,
                })
            } else {
                None
            }
        })
        .collect()
}

#[cfg(unix)]
struct RawModeGuard;

#[cfg(unix)]
impl RawModeGuard {
    fn new() -> Self {
        let _ = std::process::Command::new("stty")
            .arg("-echo")
            .arg("-icanon")
            .status();
        RawModeGuard
    }
}

#[cfg(unix)]
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .arg("icanon")
            .status();
        print!("\x1B[2J\x1B[H");
    }
}

fn print_performance_summary(
    count: usize,
    scan: Duration,
    size_calc: Duration,
    purge: Duration,
    throughput_mb_s: f64,
) {
    let scan_ms = scan.as_secs_f64() * 1000.0;
    let size_ms = size_calc.as_secs_f64() * 1000.0;
    let purge_ms = purge.as_secs_f64() * 1000.0;

    // Calcul dynamique du temps max pour adapter l'échelle des graphiques
    let max_time = [scan_ms, size_ms, purge_ms]
        .into_iter()
        .fold(0.0f64, f64::max);
    let time_scale = if max_time > 0.0 { max_time } else { 1.0 };
    let throughput_scale = if throughput_mb_s > 0.0 {
        throughput_mb_s * 1.2
    } else {
        1000.0
    };

    println!("\n=== Execution Summary ===");
    println!("Purged directories : {}", count);
    println!("Total execution time: {:.2?}", scan + size_calc + purge);
    println!();
    println!("Metrics Breakdown:");
    print_ascii_bar("Scan latency ", scan_ms, "ms", time_scale);
    print_ascii_bar("Size calculation", size_ms, "ms", time_scale);
    print_ascii_bar("Purge latency", purge_ms, "ms", time_scale);
    print_ascii_bar("Purge speed  ", throughput_mb_s, "MB/s", throughput_scale);
    println!("=========================");
}

fn print_ascii_bar(label: &str, value: f64, unit: &str, max_scale: f64) {
    let width = 25;
    let ratio = (value / max_scale).clamp(0.0, 1.0);
    let filled = cmp::max(1, (ratio * width as f64) as usize);
    let bar: String = "#".repeat(filled) + &"-".repeat(width - filled);
    println!("  {:16} | [{}] {:8.2} {}", label, bar, value, unit);
}
