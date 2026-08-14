use std::path::Path;

use anyhow::{Result, bail};
use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::Encoding;

#[derive(Debug)]
pub(crate) struct DecodedText {
    pub text: String,
    pub encoding: Option<&'static str>,
}

pub(crate) fn decode_text(bytes: &[u8], path: &Path) -> Result<DecodedText> {
    if let Some((encoding, bom_len)) = Encoding::for_bom(bytes) {
        return decode_with_encoding(&bytes[bom_len..], encoding, path, true);
    }
    if bytes.contains(&0) {
        bail!("{} is not recognized text", path.display());
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(DecodedText {
            text: text.to_string(),
            encoding: None,
        });
    }

    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, Utf8Detection::Deny);
    decode_with_encoding(bytes, encoding, path, false)
}

fn decode_with_encoding(
    bytes: &[u8],
    encoding: &'static Encoding,
    path: &Path,
    from_bom: bool,
) -> Result<DecodedText> {
    let Some(text) = encoding.decode_without_bom_handling_and_without_replacement(bytes) else {
        bail!("{} is not valid {} text", path.display(), encoding.name());
    };
    Ok(DecodedText {
        text: text.into_owned(),
        encoding: (from_bom || encoding != encoding_rs::UTF_8).then_some(encoding.name()),
    })
}
