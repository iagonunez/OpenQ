//! Fixture type import: picks the reader by file type and holds what the readers share.
//!
//! * `.json`: Open Fixture Library ([`super::ofl`]).
//! * `.gdtf`: General Device Type Format, a zip with `description.xml` ([`super::gdtf`]).
//! * `.qxf`: QLC+ fixture definition ([`super::qxf`]).
//! * `.oqfixture`: OpenQ's own fixture type file ([`super::fixlib`]).
//!
//! Readers map each channel to one [`Attribute`]. A mode can hold an attribute once per unit
//! (the fixture or one cell), so a second channel with the same attribute takes its
//! [`alternates`] (a second gobo wheel becomes `Gobo2`, a second control channel `Control2`) or is left unused with a note.

use super::model::{Attribute, ChannelDef, FixtureType};

/// What an import produced, with notes about what was left out.
#[derive(Debug)]
pub struct Imported {
    /// The fixture type.
    pub fixture: FixtureType,
    /// Things that didn't come across (shown to the user).
    pub notes: Vec<String>,
}

/// Formats a fixture file can be in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Open Fixture Library JSON.
    Ofl,
    /// GDTF (zip).
    Gdtf,
    /// QLC+ `.qxf` (XML).
    Qxf,
    /// OpenQ fixture type (`.oqfixture`).
    OpenQ,
}

impl Format {
    /// The format of a file from its extension.
    pub fn of(path: &std::path::Path) -> Option<Format> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "json" => Format::Ofl,
            "gdtf" => Format::Gdtf,
            "qxf" => Format::Qxf,
            e if e == super::fixlib::EXTENSION => Format::OpenQ,
            _ => return None,
        })
    }

    /// Every extension the importer reads.
    pub const EXTENSIONS: [&'static str; 4] = ["json", "gdtf", "qxf", super::fixlib::EXTENSION];
}

/// Largest fixture file read (GDTF zips with 3D models can be big; anything above is refused).
pub const MAX_FILE: u64 = 64 << 20;

/// Read a fixture file. `folder` is the name of the folder it is in (OFL keeps the
/// manufacturer there).
pub fn import(bytes: &[u8], format: Format, folder: &str) -> anyhow::Result<Imported> {
    let text = || std::str::from_utf8(bytes).map_err(|_| anyhow::anyhow!("the file is not text (UTF-8)"));
    match format {
        Format::Ofl => super::ofl::import(text()?, folder),
        Format::Gdtf => super::gdtf::import(bytes),
        Format::Qxf => super::qxf::import(text()?),
        Format::OpenQ => Ok(Imported { fixture: super::fixlib::read(text()?)?, notes: Vec::new() }),
    }
}

/// Read a fixture file from disk (size-capped).
pub fn import_path(path: &std::path::Path) -> anyhow::Result<Imported> {
    let format = Format::of(path).ok_or_else(|| anyhow::anyhow!("unknown fixture file type"))?;
    let len = std::fs::metadata(path)?.len();
    if len > MAX_FILE {
        anyhow::bail!("the file is too big ({} MB)", len >> 20);
    }
    let bytes = std::fs::read(path)?;
    let folder = path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    import(&bytes, format, &folder)
}

/// The attributes a repeated channel takes instead, in order (second wheels, rotations, extra
/// control/effect channels).
pub fn alternates(a: Attribute) -> &'static [Attribute] {
    use Attribute::*;
    match a {
        Gobo => &[Gobo2],
        GoboRotation => &[Gobo2Rotation],
        Prism => &[PrismRotation, Prism2],
        ColorWheel => &[ColorWheel2, ColorMacro],
        Frost => &[Frost2],
        Control => &[Control2, Control3],
        Effect => &[Effect2],
        EffectSpeed => &[Effect2Speed],
        _ => &[],
    }
}

/// Add a channel to a unit's list: its attribute if free, else the first free alternate; None
/// (and a note) when all are taken.
pub fn place(defs: &mut Vec<ChannelDef>, def: ChannelDef, what: &str, notes: &mut Vec<String>) -> bool {
    match try_place(defs, def) {
        Ok(()) => true,
        Err(def) => {
            notes.push(format!("{what}: a further {:?} channel, left unused", def.attribute));
            false
        }
    }
}

/// Like [`place`] but quiet: gives the channel back when its attribute and alternates are taken.
pub fn try_place(defs: &mut Vec<ChannelDef>, mut def: ChannelDef) -> Result<(), ChannelDef> {
    let taken = |defs: &[ChannelDef], a: Attribute| defs.iter().any(|d| d.attribute == a);
    if taken(defs, def.attribute) {
        match alternates(def.attribute).iter().copied().find(|a| !taken(defs, *a)) {
            Some(a) => def.attribute = a,
            None => return Err(def),
        }
    }
    defs.push(def);
    Ok(())
}

/// Deepest element nesting accepted in a fixture XML file.
pub const MAX_XML_DEPTH: usize = 256;

/// Refuse XML nested deeper than [`MAX_XML_DEPTH`] before parsing it: the parser recurses
/// and a hostile file would overflow the stack. Counts start tags minus end tags (comments,
/// declarations and self-closing tags don't nest); a rough count is enough for a limit.
pub fn check_depth(text: &str) -> anyhow::Result<()> {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while let Some(k) = b[i..].iter().position(|c| *c == b'<') {
        let at = i + k;
        let next = b.get(at + 1).copied().unwrap_or(0);
        let end = b[at..].iter().position(|c| *c == b'>').map_or(b.len(), |e| at + e);
        match next {
            b'/' => depth = depth.saturating_sub(1),
            b'?' | b'!' => {}
            _ if end > 0 && b.get(end - 1) == Some(&b'/') => {}
            _ => {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    anyhow::bail!("the XML is nested too deeply");
                }
            }
        }
        i = end.max(at + 1);
        if i >= b.len() {
            break;
        }
    }
    Ok(())
}

/// Drop a plain `<!DOCTYPE name>` (QLC+ files have one) so the XML parser, which refuses DTDs
/// (entity expansion), can read the rest. A DOCTYPE with an internal subset is refused.
pub fn strip_doctype(text: &str) -> anyhow::Result<std::borrow::Cow<'_, str>> {
    check_depth(text)?;
    let Some(at) = text.find("<!DOCTYPE") else { return Ok(text.into()) };
    let end = text[at..].find('>').ok_or_else(|| anyhow::anyhow!("damaged DOCTYPE"))? + at;
    if text[at..end].contains('[') {
        anyhow::bail!("XML files with DTDs are not read");
    }
    Ok(format!("{}{}", &text[..at], &text[end + 1..]).into())
}

/// A type id from a prefix and the names: `ofl/acme/spot-250`.
pub fn type_id(prefix: &str, manufacturer: &str, name: &str) -> String {
    let key = |s: &str| {
        let k: String = s.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let k = k.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
        if k.is_empty() { "unnamed".to_string() } else { k }
    };
    format!("{prefix}/{}/{}", key(manufacturer), key(name))
}

/// `#rrggbb` (or `rrggbb`) → sRGB.
pub fn hex_colour(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([b(0)?, b(2)?, b(4)?])
}

/// Guess an attribute from a channel name (QLC+ "Generic"/"Maintenance" presets, GDTF
/// attributes not in the table): None if nothing fits.
pub fn attribute_from_name(name: &str) -> Option<Attribute> {
    use Attribute::*;
    let n = name.to_lowercase();
    let has = |w: &str| n.contains(w);
    Some(if has("dimmer") || has("intensity") || has("master") {
        Dimmer
    } else if has("pan") && has("tilt") && has("speed") {
        PanTiltSpeed
    } else if has("pan") {
        Pan
    } else if has("tilt") {
        Tilt
    } else if has("shutter") || has("strobe") {
        Shutter
    } else if has("gobo") && has("rot") {
        GoboRotation
    } else if has("gobo") {
        Gobo
    } else if has("prism") && has("rot") {
        PrismRotation
    } else if has("prism") {
        Prism
    } else if has("colo") && (has("wheel") || has("whl")) {
        ColorWheel
    } else if has("macro") && has("colo") {
        ColorMacro
    } else if has("zoom") {
        Zoom
    } else if has("focus") {
        Focus
    } else if has("iris") {
        Iris
    } else if has("frost") {
        Frost
    } else if has("cto") || has("ctc") || has("ctb") || has("temperature") {
        Cto
    } else if has("fog") || has("haze") || has("smoke") {
        Fog
    } else if has("fan") {
        Fan
    } else if has("speed") {
        EffectSpeed
    } else if has("effect") || has("macro") || has("program") {
        Effect
    } else if has("control") || has("reset") || has("lamp") || has("function") || has("mode") {
        Control
    } else {
        return None;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_take_their_alternate() {
        let mut defs = Vec::new();
        let mut notes = Vec::new();
        let ch = |a| ChannelDef { attribute: a, ..Default::default() };
        assert!(place(&mut defs, ch(Attribute::Gobo), "x", &mut notes));
        assert!(place(&mut defs, ch(Attribute::Gobo), "x", &mut notes));
        assert_eq!(defs[1].attribute, Attribute::Gobo2);
        assert!(!place(&mut defs, ch(Attribute::Gobo), "x", &mut notes));
        assert_eq!(notes.len(), 1);
        for _ in 0..3 {
            assert!(place(&mut defs, ch(Attribute::Control), "x", &mut notes));
        }
        let ctl: Vec<Attribute> = defs[2..].iter().map(|d| d.attribute).collect();
        assert_eq!(ctl, [Attribute::Control, Attribute::Control2, Attribute::Control3]);
        assert!(!place(&mut defs, ch(Attribute::Control), "x", &mut notes), "a fourth is left out");
    }

    #[test]
    fn doctype() {
        assert_eq!(strip_doctype("<?xml?>\n<!DOCTYPE X>\n<X/>").unwrap(), "<?xml?>\n\n<X/>");
        assert!(strip_doctype("<!DOCTYPE x [<!ENTITY a \"b\">]><x/>").is_err());
        assert_eq!(strip_doctype("<x/>").unwrap(), "<x/>");
        assert!(check_depth(&"<a>".repeat(MAX_XML_DEPTH + 1)).is_err());
        assert!(check_depth(&"<a><b/></a>".repeat(10_000)).is_ok());
        assert!(check_depth("<a><!-- <b> --><?x?></a>").is_ok());
    }

    #[test]
    fn ids_colours_names() {
        assert_eq!(type_id("ofl", "Acme Co.", " Spot 250 "), "ofl/acme-co/spot-250");
        assert_eq!(hex_colour("#ff8000"), Some([255, 128, 0]));
        assert_eq!(hex_colour("#ff80"), None);
        assert_eq!(attribute_from_name("Gobo Wheel Rotation"), Some(Attribute::GoboRotation));
        assert_eq!(attribute_from_name("Colour wheel"), Some(Attribute::ColorWheel));
        assert_eq!(attribute_from_name("???"), None);
        assert_eq!(Format::of(std::path::Path::new("a/B.GDTF")), Some(Format::Gdtf));
    }
}
