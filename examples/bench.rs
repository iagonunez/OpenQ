//! Benchmark runner (see `openq::bench`). Usually run through `tools/bench.sh`.
//!
//!   cargo run --release --example bench -- [options]
//!
//!   --quick              smaller sizes (a quick check; not comparable with full runs)
//!   --only a,b           only these groups (engine lookup mixer timing video text waveform
//!                        workspace remote ui)
//!   --no-realtime        skip the real-time groups (mixer, timing)
//!   --out FILE           write the JSON report there
//!   --history FILE       history of complete runs: compare with the previous comparable run (and
//!                        the previous version), then add this run to it (partial runs are only
//!                        compared; --quick runs neither)
//!   --page FILE          regenerate the results section of this HTML page from the history
//!   --compare FILE       compare with this report instead of the history's previous run
//!   --tolerance PCT      allowed slowdown before a metric counts as a regression (default 15)
//!   --label TEXT         note stored in the report
//!   --revision REV       git revision stored in the report
//!
//! Exit code 2: a regression against the report it was compared with.

use openq::bench::{self, History, Options, Report, fmt_value as fmt};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut opts = Options::default();
    let (mut out, mut against, mut tolerance, mut label, mut revision) = (None, None, 15.0, String::new(), String::new());
    let (mut history_path, mut page): (Option<PathBuf>, Option<PathBuf>) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_default();
        match a.as_str() {
            "--quick" => opts.quick = true,
            "--no-realtime" => opts.no_realtime = true,
            "--only" => opts.filter = val().split(',').map(str::to_string).filter(|s| !s.is_empty()).collect(),
            "--out" => out = Some(val()),
            "--history" => history_path = Some(val().into()),
            "--page" => page = Some(val().into()),
            "--compare" => against = Some(val()),
            "--tolerance" => tolerance = val().parse().unwrap_or(15.0),
            "--label" => label = val(),
            "--revision" => revision = val(),
            "-h" | "--help" => {
                eprintln!("see the top of examples/bench.rs; groups: {}", bench::GROUPS.join(" "));
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown option {other} (try --help)");
                return ExitCode::FAILURE;
            }
        }
    }
    // Read the history first: a broken file should fail before a minute of benchmarks.
    let mut history = match history_path.as_deref().map(History::load).transpose() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("can't read the history: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    let mut report = bench::run(&opts);
    report.label = label;
    report.revision = revision;

    println!(
        "\nv{} · {} · {} · {}{}",
        report.version,
        report.machine,
        report.profile,
        report.features.join("+"),
        if report.quick { " · quick" } else { "" }
    );
    let width = report.metrics.iter().map(|m| m.name.len()).max().unwrap_or(10);
    for m in &report.metrics {
        println!("{:width$}  {:>10} {:<7}  {}", m.name, fmt(m.value), m.unit, m.note);
    }

    if let Some(path) = &out {
        match serde_json::to_string_pretty(&report).map(|s| std::fs::write(path, s)) {
            Ok(Ok(())) => eprintln!("\nsaved {path}"),
            Ok(Err(e)) => eprintln!("\ncan't write {path}: {e}"),
            Err(e) => eprintln!("\ncan't serialise the report: {e}"),
        }
    }

    // What to compare with: an explicit report, else the previous run in the history.
    let old: Option<(Report, String)> = match &against {
        Some(path) => match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())) {
            Ok(r) => Some((r, path.clone())),
            Err(e) => {
                eprintln!("can't read {path}: {e}");
                return ExitCode::FAILURE;
            }
        },
        // Partial runs (--only, --no-realtime) compare the groups they ran; --quick sizes don't compare.
        None => history.as_ref().filter(|_| !report.quick).and_then(|h| h.previous(&report)).map(|r| (r.clone(), "the previous run".into())),
    };
    let mut regressions = 0;
    if let Some((old, what)) = &old {
        regressions = print_comparison(old, &report, what, width, tolerance, true);
    } else if history.is_some() && History::accepts(&report) {
        println!("\nfirst run on this machine: nothing to compare with yet");
    } else if history.is_some() && report.quick {
        println!("\n--quick runs aren't compared with or kept in the history");
    }
    // Then a summary against the last run of the previous version.
    if let Some(prev) = history.as_ref().and_then(|h| h.previous_version(&report))
        && old.as_ref().is_none_or(|(o, _)| o.revision != prev.revision)
    {
        print_comparison(prev, &report, &format!("the previous version (v{})", prev.version), width, tolerance, false);
    }

    if let (Some(h), Some(path)) = (&mut history, &history_path) {
        if report.partial {
            eprintln!("\nnot added to {}: only some groups ran", path.display());
        } else if h.add(report) {
            match h.save(path) {
                Ok(()) => eprintln!("\nadded to {}", path.display()),
                Err(e) => eprintln!("\ncan't write {}: {e:#}", path.display()),
            }
        }
        if let Some(page) = &page {
            match bench::update_page(page, h, tolerance / 100.0) {
                Ok(()) => eprintln!("updated {}", page.display()),
                Err(e) => eprintln!("can't update {}: {e:#}", page.display()),
            }
        }
    }

    if regressions > 0 {
        println!("\n{regressions} regression(s) beyond {tolerance}% (re-run to rule out noise)");
        ExitCode::from(2)
    } else {
        if old.is_some() {
            println!("\nno regressions beyond {tolerance}%");
        }
        ExitCode::SUCCESS
    }
}

/// Print the changes from `old` to `new`; `all` lists every metric, otherwise only the ones
/// beyond the tolerance. Returns the number of (non-noisy) regressions.
fn print_comparison(old: &Report, new: &Report, what: &str, width: usize, tolerance: f64, all: bool) -> usize {
    println!(
        "\nCompared with {what}: {}{}",
        if old.revision.is_empty() { "?" } else { &old.revision },
        if old.label.is_empty() { String::new() } else { format!(", {}", old.label) }
    );
    for w in bench::comparability(old, new) {
        println!("  warning: {w}");
    }
    let mut regressions = 0;
    for c in &bench::compare(old, new, tolerance / 100.0) {
        let mark = if c.regression && c.noisy {
            "worse? (noisy)"
        } else if c.regression {
            regressions += 1;
            "SLOWER"
        } else if c.improvement {
            "faster"
        } else {
            ""
        };
        if (!all && mark.is_empty()) || (new.partial && c.new.is_none()) {
            continue;
        }
        let (o, n) = (c.old.map_or("-".into(), fmt), c.new.map_or("-".into(), fmt));
        let pct = if c.old.is_some() && c.new.is_some() { format!("{:+.0}%", c.worse_by * 100.0) } else { "new/gone".into() };
        println!("{:width$}  {o:>10} -> {n:>10} {:<7} {pct:>8}  {mark}", c.name, c.unit);
    }
    regressions
}
