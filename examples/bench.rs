//! Benchmark runner (see `openq::bench`). Usually run through `tools/bench.sh`.
//!
//!   cargo run --release --example bench -- [options]
//!
//!   --quick              smaller sizes (a quick check; not comparable with full runs)
//!   --only a,b           only these groups (engine lookup mixer timing video text waveform
//!                        workspace remote ui)
//!   --no-realtime        skip the real-time groups (mixer, timing)
//!   --out FILE           write the JSON report there
//!   --compare FILE       compare with an earlier report; exits with 2 on a regression
//!   --tolerance PCT      allowed slowdown before a metric counts as a regression (default 15)
//!   --label TEXT         note stored in the report
//!   --revision REV       git revision stored in the report

use openq::bench::{self, Options, Report};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut opts = Options::default();
    let (mut out, mut against, mut tolerance, mut label, mut revision) = (None, None, 15.0, String::new(), String::new());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_default();
        match a.as_str() {
            "--quick" => opts.quick = true,
            "--no-realtime" => opts.no_realtime = true,
            "--only" => opts.filter = val().split(',').map(str::to_string).filter(|s| !s.is_empty()).collect(),
            "--out" => out = Some(val()),
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
    let mut report = bench::run(&opts);
    report.label = label;
    report.revision = revision;

    println!("\n{} · {} · {}{}", report.machine, report.profile, report.features.join("+"), if report.quick { " · quick" } else { "" });
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

    let Some(path) = against else { return ExitCode::SUCCESS };
    let old: Report = match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("can't read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("\nCompared with {path} ({}{})", if old.revision.is_empty() { "?" } else { &old.revision }, if old.label.is_empty() { String::new() } else { format!(", {}", old.label) });
    for w in bench::comparability(&old, &report) {
        println!("  warning: {w}");
    }
    let changes = bench::compare(&old, &report, tolerance / 100.0);
    let mut regressions = 0;
    for c in &changes {
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
        let (o, n) = (c.old.map_or("-".into(), fmt), c.new.map_or("-".into(), fmt));
        let pct = if c.old.is_some() && c.new.is_some() { format!("{:+.0}%", c.worse_by * 100.0) } else { "new/gone".into() };
        println!("{:width$}  {o:>10} -> {n:>10} {:<7} {pct:>8}  {mark}", c.name, c.unit);
    }
    if regressions > 0 {
        println!("\n{regressions} regression(s) beyond {tolerance}% (re-run to rule out noise)");
        ExitCode::from(2)
    } else {
        println!("\nno regressions beyond {tolerance}%");
        ExitCode::SUCCESS
    }
}

fn fmt(v: f64) -> String {
    if v == 0.0 || v.abs() >= 100.0 {
        format!("{v:.0}")
    } else if v.abs() >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.3}")
    }
}
