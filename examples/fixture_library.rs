//! Build the fixture library shipped with OpenQ:
//! `cargo run --release --example fixture_library -- <out dir> [--gdtf <dir>] [--ofl] [--makers a,b,c]`
//!
//! `--gdtf` converts every `.gdtf` in a folder, `--ofl` downloads the Open Fixture Library
//! (all manufacturers, or only the listed keys with `--makers`). Writes one `.oqfixture` per type.
//!
//! `cargo run --release --example fixture_library -- --pack <file> <dir>…` packs the `.oqfixture`
//! files of those folders into the archive OpenQ embeds (`assets/fixtures/library.gz`).

use openq::light::{fixlib, import, oflsite};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--pack") {
        pack(&args[1..]);
        return;
    }
    let Some(out) = args.first().map(std::path::PathBuf::from) else {
        eprintln!("usage: fixture_library <out dir> [--gdtf <dir>] [--ofl] [--makers a,b]");
        std::process::exit(2);
    };
    let flag = |name: &str| args.iter().position(|a| a == name);
    let mut saved = 0;
    let mut failed = 0;
    if let Some(i) = flag("--gdtf") {
        let dir = args.get(i + 1).expect("--gdtf needs a folder");
        let mut files: Vec<_> = std::fs::read_dir(dir).expect("read dir").flatten().map(|e| e.path()).collect();
        files.sort();
        for p in files.iter().filter(|p| p.extension().and_then(|x| x.to_str()) == Some("gdtf")) {
            match import::import_path(p).and_then(|i| fixlib::sanitize(i.fixture)).and_then(|t| fixlib::save(&out, &t)) {
                Ok(f) => {
                    saved += 1;
                    println!("ok   {}", f.display());
                }
                Err(e) => {
                    failed += 1;
                    println!("FAIL {}: {e:#}", p.display());
                }
            }
        }
    }
    if flag("--ofl").is_some() {
        let makers: Option<Vec<String>> = flag("--makers").and_then(|i| args.get(i + 1)).map(|s| s.split(',').map(str::to_string).collect());
        let cat = oflsite::fetch_catalogue(&|d, n| eprint!("\rcatalogue {d}/{n}")).expect("catalogue");
        eprintln!();
        let list: Vec<_> = cat.fixtures.iter().filter(|e| makers.as_ref().is_none_or(|m| m.contains(&e.maker))).collect();
        println!("{} fixtures", list.len());
        let next = std::sync::atomic::AtomicUsize::new(0);
        let stats = parking_lot::Mutex::new((0usize, 0usize));
        std::thread::scope(|s| {
            for _ in 0..6 {
                s.spawn(|| loop {
                    let Some(e) = list.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) else { return };
                    let r = oflsite::download(e).and_then(|i| fixlib::save(&out, &i.fixture));
                    let mut st = stats.lock();
                    match r {
                        Ok(_) => st.0 += 1,
                        Err(err) => {
                            st.1 += 1;
                            println!("FAIL {}/{}: {err:#}", e.maker, e.key);
                        }
                    }
                });
            }
        });
        let st = stats.into_inner();
        saved += st.0;
        failed += st.1;
    }
    println!("saved {saved}, failed {failed}");
}

/// Compact JSON, one type per line, gzipped; sorted by id so the archive is stable.
fn pack(args: &[String]) {
    use std::io::Write;
    let (file, dirs) = args.split_first().expect("--pack <file> <dir>…");
    let mut types = std::collections::BTreeMap::new();
    for d in dirs {
        let (found, errors) = fixlib::load_dir(std::path::Path::new(d));
        for (name, e) in errors {
            eprintln!("skipped {name}: {e}");
        }
        for t in found {
            types.insert(t.id.clone(), t);
        }
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    for t in types.values() {
        writeln!(gz, "{}", serde_json::to_string(t).expect("serialise")).expect("write");
    }
    let bytes = gz.finish().expect("finish");
    std::fs::write(file, &bytes).expect("write archive");
    println!("{} types, {} bytes", types.len(), bytes.len());
}
