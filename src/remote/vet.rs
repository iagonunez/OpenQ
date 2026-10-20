//! Checks on what a replica sends before the main device takes it.
//!
//! An Edit-role device changes the show, but it must not be able to reach the main device's
//! files or crash it:
//! * every number must be finite and not absurdly large ([`MAX_NUMBER`]): JSON can't carry
//!   NaN, but a huge literal turns into infinity in an `f32` field, and wild times would
//!   overflow clock arithmetic;
//! * cue ids are unique;
//! * stage canvases stay within [`MAX_STAGE_SIDE`]; cues and stages within their counts
//!   ([`crate::limits::check_counts`]);
//! * media paths must already be in the show: replicas can't import media (the files live on the
//!   main device), so a path a replica brings in can only be an attempt to read something else.

use crate::model::{Cue, CueKind, Workspace};
use serde::Serialize;
use serde::ser;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Largest magnitude accepted for any number in an edit (about 30 000 years in seconds).
pub const MAX_NUMBER: f64 = 1e12;

/// Media files the workspace refers to.
pub fn media_paths(ws: &Workspace) -> Vec<PathBuf> {
    ws.cue_list.iter_all().into_iter().filter_map(media_path).filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).collect()
}

/// The media file a cue plays, if it has one.
fn media_path(c: &Cue) -> Option<&Path> {
    match &c.kind {
        CueKind::Audio(a) => Some(&a.path),
        CueKind::Video(v) => Some(&v.path),
        _ => None,
    }
}

/// Check every number in `value` ([`MAX_NUMBER`], finite).
pub fn numbers<T: Serialize + ?Sized>(value: &T) -> Result<(), String> {
    value.serialize(Numbers).map_err(|e| e.0)
}

/// Largest stage canvas side accepted from a replica, pixels.
pub const MAX_STAGE_SIDE: u32 = 16384;

/// Check a whole workspace from a replica against the media files the show uses now (`known`,
/// [`media_paths`]; the main device keeps them in [`crate::show::Show::lookups`]).
pub fn workspace(known: &HashSet<PathBuf>, new: &Workspace) -> Result<(), String> {
    numbers(new)?;
    crate::limits::check_counts(new)?;
    if new.stages.iter().any(|s| s.width.max(s.height) > MAX_STAGE_SIDE) {
        return Err(format!("Stages can be at most {MAX_STAGE_SIDE} pixels wide and high"));
    }
    let all = new.cue_list.iter_all();
    let mut ids = HashSet::with_capacity(all.len());
    if let Some(c) = all.iter().find(|c| !ids.insert(c.id)) {
        return Err(format!("The edit has two cues with the same id ({})", c.id));
    }
    paths(known, all)
}

/// Check changed cues from a replica against the media files the show uses now (see
/// [`workspace`]).
pub fn cues(known: &HashSet<PathBuf>, cues: &[Cue]) -> Result<(), String> {
    numbers(cues)?;
    paths(known, cues.iter())
}

fn paths<'a>(known: &HashSet<PathBuf>, cues: impl IntoIterator<Item = &'a Cue>) -> Result<(), String> {
    for c in cues {
        let Some(p) = media_path(c).filter(|p| !p.as_os_str().is_empty()) else { continue };
        if !known.contains(p) {
            return Err(format!("A replica can't add media files ({}); add them on the main device", p.display()));
        }
    }
    Ok(())
}

/// Why a value was refused.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

impl ser::Error for Refused {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Refused(msg.to_string())
    }
}

/// A serializer that writes nothing and only looks at the numbers going through it.
#[derive(Clone, Copy)]
struct Numbers;

fn number(v: f64) -> Result<(), Refused> {
    if v.is_finite() && v.abs() <= MAX_NUMBER { Ok(()) } else { Err(Refused(format!("The edit holds an invalid number ({v})"))) }
}

macro_rules! ok {
    ($($name:ident: $t:ty),* $(,)?) => {
        $(fn $name(self, _: $t) -> Result<(), Refused> { Ok(()) })*
    };
}

impl ser::Serializer for Numbers {
    type Ok = ();
    type Error = Refused;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    ok!(serialize_bool: bool, serialize_i8: i8, serialize_i16: i16, serialize_i32: i32, serialize_i64: i64, serialize_i128: i128,
        serialize_u8: u8, serialize_u16: u16, serialize_u32: u32, serialize_u64: u64, serialize_u128: u128, serialize_char: char,
        serialize_str: &str, serialize_bytes: &[u8], serialize_unit_struct: &'static str);

    fn serialize_f32(self, v: f32) -> Result<(), Refused> {
        number(v as f64)
    }
    fn serialize_f64(self, v: f64) -> Result<(), Refused> {
        number(v)
    }
    fn serialize_none(self) -> Result<(), Refused> {
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<(), Refused> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), Refused> {
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, _: &'static str) -> Result<(), Refused> {
        Ok(())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<(), Refused> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, _: &'static str, _: u32, _: &'static str, v: &T) -> Result<(), Refused> {
        v.serialize(self)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self, Refused> {
        Ok(self)
    }
    fn serialize_tuple(self, _: usize) -> Result<Self, Refused> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self, Refused> {
        Ok(self)
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self, Refused> {
        Ok(self)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self, Refused> {
        Ok(self)
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self, Refused> {
        Ok(self)
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self, Refused> {
        Ok(self)
    }
    fn is_human_readable(&self) -> bool {
        true
    }
}

impl ser::SerializeSeq for Numbers {
    type Ok = ();
    type Error = Refused;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Refused> {
        v.serialize(*self)
    }
    fn end(self) -> Result<(), Refused> {
        Ok(())
    }
}

impl ser::SerializeTuple for Numbers {
    type Ok = ();
    type Error = Refused;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Refused> {
        v.serialize(*self)
    }
    fn end(self) -> Result<(), Refused> {
        Ok(())
    }
}

impl ser::SerializeTupleStruct for Numbers {
    type Ok = ();
    type Error = Refused;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Refused> {
        v.serialize(*self)
    }
    fn end(self) -> Result<(), Refused> {
        Ok(())
    }
}

impl ser::SerializeTupleVariant for Numbers {
    type Ok = ();
    type Error = Refused;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Refused> {
        v.serialize(*self)
    }
    fn end(self) -> Result<(), Refused> {
        Ok(())
    }
}

impl ser::SerializeMap for Numbers {
    type Ok = ();
    type Error = Refused;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, k: &T) -> Result<(), Refused> {
        k.serialize(*self)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Refused> {
        v.serialize(*self)
    }
    fn end(self) -> Result<(), Refused> {
        Ok(())
    }
}

impl ser::SerializeStruct for Numbers {
    type Ok = ();
    type Error = Refused;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, _: &'static str, v: &T) -> Result<(), Refused> {
        v.serialize(*self)
    }
    fn end(self) -> Result<(), Refused> {
        Ok(())
    }
}

impl ser::SerializeStructVariant for Numbers {
    type Ok = ();
    type Error = Refused;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, _: &'static str, v: &T) -> Result<(), Refused> {
        v.serialize(*self)
    }
    fn end(self) -> Result<(), Refused> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioCue, Cue, CueKind};

    #[test]
    fn numbers_and_paths() {
        let mut ws = Workspace::default();
        let audio = Cue::new(CueKind::Audio(AudioCue { path: "/show/a.wav".into(), ..Default::default() }));
        ws.cue_list.cues.push(audio.clone());
        let known: HashSet<PathBuf> = media_paths(&ws).into_iter().collect();
        assert!(workspace(&known, &ws).is_ok());

        let mut wild = ws.clone();
        wild.cue_list.cues[0].pre_wait = 1e300;
        assert!(workspace(&known, &wild).is_err(), "huge time");
        let mut nan = ws.clone();
        nan.settings.master_volume_db = f32::INFINITY;
        assert!(workspace(&known, &nan).is_err(), "infinite level");

        // A replica can reuse the show's files but not bring in new ones.
        let mut copy = audio.clone();
        copy.id = crate::model::CueId::new_v4();
        assert!(cues(&known, &[copy.clone()]).is_ok());
        if let CueKind::Audio(a) = &mut copy.kind {
            a.path = "/etc/shadow".into();
        }
        assert!(cues(&known, &[copy.clone()]).is_err());
        let mut added = ws.clone();
        added.cue_list.cues.push(copy);
        assert!(workspace(&known, &added).is_err());

        let mut twice = ws.clone();
        twice.cue_list.cues.push(audio.clone());
        assert!(workspace(&known, &twice).is_err(), "same id twice");
    }
}
