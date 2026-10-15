//! Search and download fixtures from the Open Fixture Library website
//! (<https://open-fixture-library.org>).
//!
//! The catalogue (every manufacturer and fixture name, with categories) comes from the site's API:
//! one request for the manufacturers, then one per manufacturer, a few at a time. It is cached in
//! `<config>/ofl-catalogue.json` and fetched again when older than [`MAX_AGE`] or on request, so
//! searching works offline and doesn't hit the site on every keystroke. A downloaded fixture is the
//! site's OFL JSON, read by [`super::ofl`] like an imported file and checked with
//! [`super::fixlib::sanitize`].
//!
//! Everything here blocks on the network: [`Browser`] runs it on its own threads. Without the
//! `fixture-download` feature every request fails (the UI hides the window).

use super::import::Imported;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The site.
pub const SITE: &str = "https://open-fixture-library.org";
/// Cached catalogues older than this are fetched again when the browser opens.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// Largest API answer or fixture file read.
const MAX_BODY: u64 = 8 << 20;
/// Manufacturer lists fetched at the same time.
const PARALLEL: usize = 6;
/// Most manufacturers read from the API (the site has ~150).
const MAX_MANUFACTURERS: usize = 2000;

/// One fixture of the catalogue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Manufacturer key (the site's folder name, e.g. `martin`).
    pub maker: String,
    /// Manufacturer name.
    pub maker_name: String,
    /// Fixture key (e.g. `mac-aura`).
    pub key: String,
    /// Fixture name.
    pub name: String,
    /// OFL categories ("Moving Head", "Color Changer"…).
    #[serde(default)]
    pub categories: Vec<String>,
}

impl Entry {
    /// The fixture's page on the site.
    pub fn url(&self) -> String {
        format!("{SITE}/{}/{}", self.maker, self.key)
    }

    /// The id the imported type gets (to tell whether it is already in the library or show).
    pub fn type_id(&self) -> String {
        super::import::type_id("ofl", &self.maker_name, &self.name)
    }
}

/// Every fixture on the site.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalogue {
    /// When it was fetched (seconds since the Unix epoch).
    pub fetched: u64,
    /// Sorted by manufacturer name, then fixture name.
    pub fixtures: Vec<Entry>,
}

impl Catalogue {
    /// Fixtures matching every word of `query` (in manufacturer, name, key or categories; case
    /// and punctuation ignored), at most `limit`. An empty query matches everything.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Entry> {
        let words: Vec<String> = query.split_whitespace().map(fold).filter(|w| !w.is_empty()).collect();
        self.fixtures
            .iter()
            .filter(|e| {
                if words.is_empty() {
                    return true;
                }
                let hay = fold(&format!("{} {} {} {} {}", e.maker_name, e.maker, e.name, e.key, e.categories.join(" ")));
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .take(limit)
            .collect()
    }

    /// Number of manufacturers.
    pub fn makers(&self) -> usize {
        let mut v: Vec<&str> = self.fixtures.iter().map(|e| e.maker.as_str()).collect();
        v.dedup();
        v.len()
    }

    /// Whether it should be fetched again.
    pub fn is_old(&self) -> bool {
        now().saturating_sub(self.fetched) > MAX_AGE.as_secs()
    }
}

/// Lowercase letters and digits only (so "MAC-Aura" matches "mac aura").
fn fold(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Keys go into URLs: only what the site uses.
fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 128 && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn clip(s: &str) -> String {
    s.chars().take(200).collect()
}

/// An HTTP agent with timeouts (the site can be slow, never stuck forever).
#[cfg(feature = "fixture-download")]
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(concat!("OpenQ/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

#[cfg(feature = "fixture-download")]
fn get(agent: &ureq::Agent, url: &str) -> anyhow::Result<Vec<u8>> {
    let mut r = agent.get(url).call().map_err(|e| anyhow::anyhow!("{url}: {e}"))?;
    Ok(r.body_mut().with_config().limit(MAX_BODY).read_to_vec()?)
}

#[cfg(not(feature = "fixture-download"))]
struct NoAgent;

#[cfg(not(feature = "fixture-download"))]
fn agent() -> NoAgent {
    NoAgent
}

#[cfg(not(feature = "fixture-download"))]
fn get(_: &NoAgent, _: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::bail!("OpenQ was built without fixture downloads")
}

/// Read the manufacturer list answer: (key, name) pairs.
fn parse_makers(json: &[u8]) -> anyhow::Result<Vec<(String, String)>> {
    let v: serde_json::Value = serde_json::from_slice(json)?;
    let o = v.as_object().ok_or_else(|| anyhow::anyhow!("unexpected manufacturer list"))?;
    Ok(o.iter()
        .filter(|(k, _)| valid_key(k))
        .take(MAX_MANUFACTURERS)
        .map(|(k, m)| (k.clone(), clip(m.get("name").and_then(|n| n.as_str()).unwrap_or(k))))
        .collect())
}

/// Read one manufacturer's answer: its fixtures.
fn parse_maker(json: &[u8], maker: &str, maker_name: &str) -> anyhow::Result<Vec<Entry>> {
    let v: serde_json::Value = serde_json::from_slice(json)?;
    let list = v.get("fixtures").and_then(|f| f.as_array()).ok_or_else(|| anyhow::anyhow!("{maker}: no fixture list"))?;
    Ok(list
        .iter()
        .filter_map(|f| {
            let key = f.get("key")?.as_str()?;
            if !valid_key(key) {
                return None;
            }
            let name = f.get("name").and_then(|n| n.as_str()).unwrap_or(key);
            let categories = f.get("categories").and_then(|c| c.as_array()).map(|c| c.iter().filter_map(|x| x.as_str()).take(8).map(clip).collect()).unwrap_or_default();
            Some(Entry { maker: maker.into(), maker_name: maker_name.into(), key: key.into(), name: clip(name), categories })
        })
        .collect())
}

/// Fetch the whole catalogue from the site (blocks; seconds). `progress(done, total)` is called
/// after each manufacturer.
pub fn fetch_catalogue(progress: &(dyn Fn(usize, usize) + Sync)) -> anyhow::Result<Catalogue> {
    let agent = agent();
    let makers = parse_makers(&get(&agent, &format!("{SITE}/api/v1/manufacturers"))?)?;
    if makers.is_empty() {
        anyhow::bail!("the site lists no manufacturers");
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let out: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..PARALLEL.min(makers.len()) {
            s.spawn(|| {
                use std::sync::atomic::Ordering::Relaxed;
                while let Some((key, name)) = makers.get(next.fetch_add(1, Relaxed)) {
                    if failed.lock().is_some() {
                        return;
                    }
                    let r = get(&agent, &format!("{SITE}/api/v1/manufacturers/{key}")).and_then(|b| parse_maker(&b, key, name));
                    match r {
                        Ok(v) => out.lock().extend(v),
                        Err(e) => {
                            failed.lock().get_or_insert(e);
                            return;
                        }
                    }
                    progress(done.fetch_add(1, Relaxed) + 1, makers.len());
                }
            });
        }
    });
    if let Some(e) = failed.into_inner() {
        return Err(e);
    }
    let mut fixtures = out.into_inner();
    fixtures.sort_by(|a, b| (a.maker_name.to_lowercase(), a.name.to_lowercase()).cmp(&(b.maker_name.to_lowercase(), b.name.to_lowercase())));
    Ok(Catalogue { fetched: now(), fixtures })
}

/// Download one fixture and read it (blocks).
pub fn download(e: &Entry) -> anyhow::Result<Imported> {
    if !valid_key(&e.maker) || !valid_key(&e.key) {
        anyhow::bail!("bad fixture key");
    }
    let bytes = get(&agent(), &format!("{SITE}/{}/{}.json", e.maker, e.key))?;
    let mut imp = super::import::import(&bytes, super::import::Format::Ofl, &e.maker_name)?;
    imp.fixture = super::fixlib::sanitize(imp.fixture)?;
    Ok(imp)
}

/// Where the catalogue is cached.
pub fn cache_path() -> Option<PathBuf> {
    crate::remote::config_dir().map(|d| d.join("ofl-catalogue.json"))
}

/// The cached catalogue, if any.
pub fn load_cache() -> Option<Catalogue> {
    let path = cache_path()?;
    if std::fs::metadata(&path).ok()?.len() > 4 * MAX_BODY {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn save_cache(c: &Catalogue) -> anyhow::Result<()> {
    let path = cache_path().ok_or_else(|| anyhow::anyhow!("no config folder"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::recovery::write_atomic(&path, &serde_json::to_vec(c)?)
}

/// What the browser has right now.
#[derive(Default, Clone)]
pub struct State {
    /// The catalogue (cached or fetched).
    pub catalogue: Option<Arc<Catalogue>>,
    /// A fetch is running: (manufacturers done, total).
    pub fetching: Option<(usize, usize)>,
    /// The last fetch failed.
    pub error: Option<String>,
    /// Fixtures being downloaded (type ids).
    pub downloading: Vec<String>,
}

/// The catalogue loaded and fetched in the background, plus downloads. Cheap to clone (shared).
#[derive(Clone, Default)]
pub struct Browser {
    state: Arc<Mutex<State>>,
    started: Arc<std::sync::atomic::AtomicBool>,
}

impl Browser {
    /// A copy of the current state.
    pub fn state(&self) -> State {
        self.state.lock().clone()
    }

    /// Load the cache (once), then fetch when there is none or it is old. `repaint` is called
    /// whenever the state changes.
    pub fn open(&self, repaint: impl Fn() + Send + Sync + 'static) {
        if self.started.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let me = self.clone();
        self.spawn("openq-ofl", move || {
            let cached = load_cache();
            let fetch = cached.as_ref().is_none_or(Catalogue::is_old);
            me.state.lock().catalogue = cached.map(Arc::new);
            repaint();
            if fetch {
                me.fetch(&repaint);
            }
        });
    }

    /// Fetch the catalogue again now (unless a fetch is running).
    pub fn refresh(&self, repaint: impl Fn() + Send + Sync + 'static) {
        if self.state.lock().fetching.is_some() {
            return;
        }
        let me = self.clone();
        self.spawn("openq-ofl", move || me.fetch(&repaint));
    }

    fn fetch(&self, repaint: &(dyn Fn() + Send + Sync)) {
        {
            let mut s = self.state.lock();
            if s.fetching.is_some() {
                return;
            }
            s.fetching = Some((0, 0));
            s.error = None;
        }
        repaint();
        let progress = |done, total| {
            self.state.lock().fetching = Some((done, total));
            repaint();
        };
        let r = crate::recovery::catch(|| fetch_catalogue(&progress));
        let mut s = self.state.lock();
        s.fetching = None;
        match r {
            Ok(c) => {
                if let Err(e) = save_cache(&c) {
                    log::warn!("can't cache the Open Fixture Library catalogue: {e:#}");
                }
                s.catalogue = Some(Arc::new(c));
            }
            Err(e) => s.error = Some(format!("{e:#}")),
        }
        drop(s);
        repaint();
    }

    /// Download fixtures; `done` gets each result (on a worker thread).
    pub fn download(&self, entries: Vec<Entry>, done: impl Fn(&Entry, anyhow::Result<Imported>) + Send + 'static) {
        let ids: Vec<String> = entries.iter().map(Entry::type_id).collect();
        {
            let mut s = self.state.lock();
            if ids.iter().all(|id| s.downloading.contains(id)) {
                return;
            }
            s.downloading.extend(ids.iter().cloned());
        }
        let me = self.clone();
        self.spawn("openq-ofl-download", move || {
            for (e, id) in entries.iter().zip(&ids) {
                let r = crate::recovery::catch(|| download(e));
                me.state.lock().downloading.retain(|x| x != id);
                done(e, r);
            }
        });
    }

    fn spawn(&self, name: &str, f: impl FnOnce() + Send + 'static) {
        if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
            self.state.lock().error = Some(e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_api_answers_and_searches() {
        let makers = br#"{"$schema":"x","martin":{"name":"Martin","fixtureCount":2},"../evil":{"name":"Bad"},"robe":{"name":"Robe"}}"#;
        let m = parse_makers(makers).unwrap();
        assert_eq!(m, vec![("martin".to_string(), "Martin".to_string()), ("robe".to_string(), "Robe".to_string())]);
        let list = br#"{"name":"Martin","fixtures":[{"key":"mac-aura","name":"MAC Aura","categories":["Moving Head","Color Changer"]},
            {"key":"atomic-3000","name":"Atomic 3000","categories":["Strobe"]},{"key":"a/b","name":"Bad"}]}"#;
        let fixtures = parse_maker(list, "martin", "Martin").unwrap();
        assert_eq!(fixtures.len(), 2);
        let c = Catalogue { fetched: now(), fixtures };
        assert_eq!(c.search("mac-AURA", 10)[0].key, "mac-aura");
        assert_eq!(c.search("martin strobe", 10)[0].key, "atomic-3000");
        assert!(c.search("clay paky", 10).is_empty());
        assert_eq!(c.search("", 1).len(), 1);
        assert_eq!(c.makers(), 1);
        assert!(!c.is_old());
        assert_eq!(c.fixtures[0].type_id(), crate::light::import::type_id("ofl", "Martin", "MAC Aura"));
        assert_eq!(c.fixtures[0].url(), "https://open-fixture-library.org/martin/mac-aura");
    }

    /// Talks to the real site: `cargo test oflsite -- --ignored`.
    #[test]
    #[ignore]
    #[cfg(feature = "fixture-download")]
    fn downloads_from_the_site() {
        let e = Entry { maker: "martin".into(), maker_name: "Martin".into(), key: "mac-aura".into(), name: "MAC Aura".into(), categories: vec![] };
        let imp = download(&e).unwrap();
        assert_eq!(imp.fixture.id, e.type_id());
        assert!(!imp.fixture.modes.is_empty());
        let c = fetch_catalogue(&|_, _| {}).unwrap();
        assert!(c.fixtures.len() > 300 && c.makers() > 50, "{} fixtures", c.fixtures.len());
    }
}
