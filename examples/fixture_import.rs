//! Read fixture files with OpnQ's importers and print what came across:
//! `cargo run --example fixture_import -- <file.json|.gdtf|.qxf|.oqfixture>…`

fn main() {
    let mut failed = false;
    for arg in std::env::args().skip(1) {
        let path = std::path::Path::new(&arg);
        match opnq::light::import::import_path(path) {
            Ok(imp) => {
                let t = &imp.fixture;
                println!("{arg}\n  {} · {} {} · {} modes", t.id, t.manufacturer, t.name, t.modes.len());
                for m in &t.modes {
                    let ranges: usize = m.all_channels().map(|c| c.ranges.len()).sum();
                    println!(
                        "  mode \"{}\": {} slots, {} master channels, {} cells, {ranges} named ranges",
                        m.name,
                        m.footprint(),
                        m.channels.len(),
                        m.heads.len()
                    );
                    for c in &m.channels {
                        let names: Vec<&str> = c.ranges.iter().take(6).map(|r| r.name.as_str()).collect();
                        println!("    {:>3} {:?}{} {}", c.offset + 1, c.attribute, if c.fine.is_some() { " (16-bit)" } else { "" }, names.join(" | "));
                    }
                }
                for n in &imp.notes {
                    println!("  note: {n}");
                }
            }
            Err(e) => {
                failed = true;
                println!("{arg}\n  error: {e:#}");
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
