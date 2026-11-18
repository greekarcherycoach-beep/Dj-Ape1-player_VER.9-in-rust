//! Audio tag reading (replaces jsmediatags). Output mimics jsmediatags' `tags` object
//! so the UI code works unchanged.

use base64::Engine;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::PictureType;
use lofty::tag::{Accessor, ItemKey, Tag};
use serde_json::{json, Map, Value};

pub fn read(path: &str) -> Value {
    let p = path.to_string();
    std::panic::catch_unwind(move || inner(&p))
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}))
}

fn s(v: Option<impl AsRef<str>>) -> Option<Value> {
    v.map(|x| x.as_ref().trim().to_string()).filter(|x| !x.is_empty()).map(Value::String)
}

fn inner(path: &str) -> Option<Value> {
    let tf = lofty::read_from_path(path).ok()?;
    let props = tf.properties();
    let mut o = Map::new();
    o.insert("__dur".into(), json!(props.duration().as_secs_f64()));
    o.insert("__sr".into(), json!(props.sample_rate().unwrap_or(0)));
    o.insert("__bits".into(), json!(props.bit_depth().unwrap_or(0)));

    let tag: &Tag = match tf.primary_tag().or_else(|| tf.first_tag()) {
        Some(t) => t,
        None => return Some(Value::Object(o)),
    };
    if let Some(v) = s(tag.title()) { o.insert("title".into(), v); }
    if let Some(v) = s(tag.artist()) { o.insert("artist".into(), v); }
    if let Some(v) = s(tag.album()) { o.insert("album".into(), v); }

    let genres: Vec<&str> = tag.get_strings(&ItemKey::Genre).collect();
    if !genres.is_empty() {
        o.insert("genre".into(), Value::String(genres.join(";")));
    }
    let year = tag
        .year()
        .map(|y| y.to_string())
        .or_else(|| tag.get_string(&ItemKey::RecordingDate).map(|d| d.chars().take(4).collect()));
    if let Some(y) = year { o.insert("year".into(), Value::String(y)); }
    if let Some(t) = tag.track() { o.insert("track".into(), Value::String(t.to_string())); }
    if let Some(c) = s(tag.comment()) { o.insert("comment".into(), json!({ "text": c })); }
    if let Some(k) = tag.get_string(&ItemKey::InitialKey) { o.insert("TKEY".into(), json!({ "data": k })); }
    let bpm = tag.get_string(&ItemKey::IntegerBpm).or_else(|| tag.get_string(&ItemKey::Bpm));
    if let Some(b) = bpm { o.insert("TBPM".into(), json!({ "data": b })); }

    let pic = tag
        .pictures()
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| tag.pictures().first());
    if let Some(p) = pic {
        if p.data().len() < 8 * 1024 * 1024 {
            let fmt = p.mime_type().map(|m| m.as_str().to_string()).unwrap_or_else(|| "image/jpeg".into());
            o.insert(
                "picture".into(),
                json!({ "format": fmt, "b64": base64::engine::general_purpose::STANDARD.encode(p.data()) }),
            );
        }
    }
    Some(Value::Object(o))
}
