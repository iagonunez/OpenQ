//! A small zip reader for GDTF files: finds one entry by name and inflates it, with a cap on
//! the output size (files come from anywhere, so a zip bomb must not exhaust memory).
//! Stored and deflate entries only; no zip64, no encryption. [`write()`] makes a zip (MVR export).

use std::io::Read;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Names of the entries (in directory order).
pub fn names(zip: &[u8]) -> anyhow::Result<Vec<String>> {
    Ok(entries(zip)?.into_iter().map(|e| e.name).collect())
}

struct Entry {
    name: String,
    method: u16,
    compressed: u32,
    size: u32,
    local: u32,
}

fn entries(zip: &[u8]) -> anyhow::Result<Vec<Entry>> {
    let bad = || anyhow::anyhow!("not a valid zip file");
    // End of central directory: 22 bytes + up to 64 KB of comment, from the end.
    let lo = zip.len().saturating_sub(22 + 0xffff);
    let eocd = (lo..zip.len().saturating_sub(21)).rev().find(|&i| u32_at(zip, i) == Some(0x0605_4b50)).ok_or_else(bad)?;
    let count = u16_at(zip, eocd + 10).ok_or_else(bad)? as usize;
    let mut at = u32_at(zip, eocd + 16).ok_or_else(bad)? as usize;
    if count == 0xffff || at == 0xffff_ffff_usize {
        anyhow::bail!("zip64 files are not supported");
    }
    let mut out = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        if u32_at(zip, at) != Some(0x0201_4b50) {
            return Err(bad());
        }
        let n = u16_at(zip, at + 28).ok_or_else(bad)? as usize;
        let extra = u16_at(zip, at + 30).ok_or_else(bad)? as usize;
        let comment = u16_at(zip, at + 32).ok_or_else(bad)? as usize;
        let name = zip.get(at + 46..at + 46 + n).ok_or_else(bad)?;
        out.push(Entry {
            name: String::from_utf8_lossy(name).replace('\\', "/"),
            method: u16_at(zip, at + 10).ok_or_else(bad)?,
            compressed: u32_at(zip, at + 20).ok_or_else(bad)?,
            size: u32_at(zip, at + 24).ok_or_else(bad)?,
            local: u32_at(zip, at + 42).ok_or_else(bad)?,
        });
        at += 46 + n + extra + comment;
    }
    Ok(out)
}

/// The contents of the entry named `name` (ASCII case-insensitive), at most `max` bytes.
pub fn read(zip: &[u8], name: &str, max: usize) -> anyhow::Result<Vec<u8>> {
    let bad = || anyhow::anyhow!("damaged zip entry \"{name}\"");
    let e = entries(zip)?.into_iter().find(|e| e.name.eq_ignore_ascii_case(name)).ok_or_else(|| anyhow::anyhow!("\"{name}\" is missing"))?;
    if e.size as usize > max {
        anyhow::bail!("\"{name}\" is too big");
    }
    let l = e.local as usize;
    if u32_at(zip, l) != Some(0x0403_4b50) {
        return Err(bad());
    }
    let start = l + 30 + u16_at(zip, l + 26).ok_or_else(bad)? as usize + u16_at(zip, l + 28).ok_or_else(bad)? as usize;
    let data = zip.get(start..start + e.compressed as usize).ok_or_else(bad)?;
    let mut out = Vec::new();
    match e.method {
        0 => out.extend_from_slice(data),
        8 => {
            flate2::read::DeflateDecoder::new(data).take(max as u64 + 1).read_to_end(&mut out).map_err(|_| bad())?;
        }
        m => anyhow::bail!("zip compression method {m} is not supported"),
    }
    if out.len() > max {
        anyhow::bail!("\"{name}\" is too big");
    }
    Ok(out)
}

/// A zip archive of `(name, contents)` entries, deflated (no zip64: under 4 GB in all).
pub fn write(entries: &[(&str, &[u8])]) -> anyhow::Result<Vec<u8>> {
    use std::io::Write;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(data)?;
        let packed = z.finish()?;
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let at = u32::try_from(out.len())?;
        let (size, packed_size) = (u32::try_from(data.len())?, u32::try_from(packed.len())?);
        // Version 2.0, UTF-8 names (bit 11), deflate, no time stamp.
        let fields = |h: &mut Vec<u8>| {
            h.extend_from_slice(&20u16.to_le_bytes());
            h.extend_from_slice(&0x0800u16.to_le_bytes());
            h.extend_from_slice(&8u16.to_le_bytes());
            h.extend_from_slice(&[0, 0, 0x21, 0]); // 1980-01-01 00:00
            h.extend_from_slice(&crc.sum().to_le_bytes());
            h.extend_from_slice(&packed_size.to_le_bytes());
            h.extend_from_slice(&size.to_le_bytes());
            h.extend_from_slice(&(name.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // extra
        };
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        fields(&mut out);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&packed);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // made by
        fields(&mut central);
        central.extend_from_slice(&[0; 6]); // comment length, disk, internal attributes
        central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        central.extend_from_slice(&at.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let at = u32::try_from(out.len())?;
    let n = u16::try_from(entries.len())?;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&u32::try_from(central.len())?.to_le_bytes());
    out.extend_from_slice(&at.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A zip with the given entries, deflated (for tests of the GDTF reader).
    pub(crate) fn make(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(data).unwrap();
            let comp = enc.finish().unwrap();
            let local = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0, 8, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&0u32.to_le_bytes()); // crc (not checked)
            out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&comp);
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 8, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&local.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let at = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn written_zips_read_back() {
        let big = vec![b'x'; 50_000];
        let zip = write(&[("GeneralSceneDescription.xml", b"<GeneralSceneDescription/>"), ("a/b.gdtf", &big)]).unwrap();
        assert_eq!(names(&zip).unwrap(), ["GeneralSceneDescription.xml", "a/b.gdtf"]);
        assert_eq!(read(&zip, "a/b.gdtf", 60_000).unwrap(), big);
        assert!(zip.len() < 2000, "deflated");
    }

    #[test]
    fn reads_entries_and_caps_size() {
        let big = vec![b'a'; 100_000];
        let zip = make(&[("description.xml", b"<GDTF/>"), ("models/x.3ds", &big)]);
        assert_eq!(names(&zip).unwrap(), ["description.xml", "models/x.3ds"]);
        assert_eq!(read(&zip, "Description.XML", 1000).unwrap(), b"<GDTF/>");
        assert!(read(&zip, "models/x.3ds", 1000).is_err(), "over the cap");
        assert!(read(&zip, "missing", 1000).is_err());
        assert!(read(b"garbage", "x", 10).is_err());
        let mut cut = zip.clone();
        cut.truncate(40);
        assert!(names(&cut).is_err());
    }
}
