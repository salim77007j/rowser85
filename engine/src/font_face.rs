//! @font-face loading: font bytes → fontdb faces → CSS family aliases.
//!
//! Formats: TTF/OTF pass through; WOFF1 and WOFF2 decompress via `wuff`
//! (zlib / brotli + table reassembly, including the glyf/loca transform).
//! After loading, the face's real family name is read out of fontdb and
//! registered as an alias for the CSS family name, so font matching at
//! shaping time (`layout::text::resolve_font_stack`) prefers the author's
//! face — mirroring CSS font matching where @font-face shadows local fonts.

use cosmic_text::FontSystem;

/// Number of faces currently registered in the font system.
fn face_count(font_system: &FontSystem) -> usize {
    font_system.db().faces().count()
}

/// The primary family name of a fontdb face (English preferred).
fn face_family(face: &cosmic_text::fontdb::FaceInfo) -> String {
    // fontdb guarantees the first family is English US; prefer it.
    face.families.first().map(|(name, _)| name.clone()).unwrap_or_default()
}

/// Decompresses `bytes` into an sfnt (TTF/OTF) payload.
fn to_sfnt(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() >= 4 {
        match &bytes[..4] {
            b"OTTO" | b"ttcf" | b"\x00\x01\x00\x00" | b"true" => return Ok(bytes.to_vec()),
            b"wOFF" => {
                return wuff::decompress_woff1(bytes).map_err(|e| format!("woff1: {e:?}"));
            }
            b"wOF2" => {
                return wuff::decompress_woff2(bytes).map_err(|e| format!("woff2: {e:?}"));
            }
            _ => {}
        }
    }
    Err("unrecognized font signature".to_owned())
}

/// Registers one web font: decompress, load into fontdb, alias the CSS
/// family name to the face's real family. Returns true when the family is
/// now available for shaping.
pub fn register_font_bytes(font_system: &mut FontSystem, css_family: &str, bytes: &[u8]) -> bool {
    let css_family = css_family.trim().trim_matches('"');
    if css_family.is_empty() {
        return false;
    }
    let sfnt = match to_sfnt(bytes) {
        Ok(sfnt) => sfnt,
        Err(err) => {
            log::warn!("@font-face {css_family}: decode failed ({err})");
            return false;
        }
    };
    let before = face_count(font_system);
    font_system.db_mut().load_font_data(sfnt);
    let after = face_count(font_system);
    if after <= before {
        log::warn!("@font-face {css_family}: no faces loaded");
        return false;
    }
    // The newly appended faces carry the real family name(s).
    let mut real_family: Option<String> = None;
    for face in font_system.db().faces().skip(before) {
        let name = face_family(face);
        if !name.is_empty() {
            real_family = Some(name);
            break;
        }
    }
    match real_family {
        Some(real) => {
            rowser_layout::text::register_web_font(css_family, &real);
            log::debug!("@font-face '{css_family}' -> '{real}' ({} faces)", after - before);
            true
        }
        None => {
            log::warn!("@font-face {css_family}: loaded faces have no family name");
            false
        }
    }
}
