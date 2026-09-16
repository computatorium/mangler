//! Lossless transport of JavaScript UTF-16 source into a UTF-8 parser.
//! A missing word over two private-use characters marks raw surrogate units.
//! The word is absent both literally and after JavaScript escape decoding, so
//! source-authored private-use characters cannot be mistaken for transport data.
use crate::{Compiled, Const};
use std::collections::{BTreeMap, HashSet};
use swc_core::ecma::{ast::Str, visit::VisitMut};

/// Parser-safe source marker and the original unpaired UTF-16 code unit.
pub type SourceTextMap = Vec<(String, u16)>;

pub struct Normalized {
    pub fragments: Vec<String>,
    pub replacements: SourceTextMap,
}

/// Restore one string without decoding or replacing existing lone surrogates.
pub fn restore_units(units: &[u16], map: &SourceTextMap) -> Vec<u16> {
    if map.is_empty() {
        return units.to_vec();
    }
    let replacements: Vec<_> = map
        .iter()
        .map(|(marker, unit)| (marker.encode_utf16().collect::<Vec<_>>(), *unit))
        .collect();
    let mut result = Vec::with_capacity(units.len());
    let mut offset = 0;
    while offset < units.len() {
        if let Some((marker, unit)) = replacements
            .iter()
            .find(|(marker, _)| units[offset..].starts_with(marker))
        {
            result.push(*unit);
            offset += marker.len();
        } else {
            result.push(units[offset]);
            offset += 1;
        }
    }
    result
}

/// Restore this frame only. Nested compilation applies the same map itself.
pub fn restore_constants(compiled: &mut Compiled, map: &SourceTextMap) {
    if map.is_empty() {
        return;
    }
    let text = |value: &str| restore_units(&value.encode_utf16().collect::<Vec<_>>(), map);
    for constant in &mut compiled.consts {
        match constant {
            Const::Str(value) => *constant = Const::Utf16(text(value)),
            Const::Utf16(value) => *value = restore_units(value, map),
            Const::RegExp { pattern, flags } => {
                *constant = Const::RegExpUtf16 {
                    pattern: text(pattern),
                    flags: flags.clone(),
                }
            }
            Const::RegExpUtf16 { pattern, .. } => *pattern = restore_units(pattern, map),
            Const::TemplateObject { cooked, raw } => {
                *constant = Const::TemplateObjectUtf16 {
                    cooked: cooked
                        .iter()
                        .map(|value| value.as_deref().map(text))
                        .collect(),
                    raw: raw.iter().map(|value| text(value)).collect(),
                }
            }
            Const::TemplateObjectUtf16 { cooked, raw } => {
                for value in cooked.iter_mut().flatten().chain(raw.iter_mut()) {
                    *value = restore_units(value, map);
                }
            }
            _ => {}
        }
    }
}

/// Restore string AST values before shared frontend code generation. Templates
/// and regex raw text remain parser-safe until their bytecode constants form.
pub struct RestoreStringLiterals<'a>(pub &'a SourceTextMap);
impl VisitMut for RestoreStringLiterals<'_> {
    fn visit_mut_str(&mut self, value: &mut Str) {
        if self.0.is_empty() {
            return;
        }
        let before: Vec<_> = value.value.to_ill_formed_utf16().collect();
        let after = restore_units(&before, self.0);
        if before != after {
            value.value = swc_core::atoms::wtf8::Wtf8Buf::from_ill_formed_utf16(&after).into();
            value.raw = None;
        }
    }
}

pub fn normalize(fragments: &[Vec<u16>]) -> Normalized {
    let decoded: Vec<Vec<Result<char, u16>>> = fragments
        .iter()
        .map(|units| {
            char::decode_utf16(units.iter().copied())
                .map(|value| value.map_err(|error| error.unpaired_surrogate()))
                .collect()
        })
        .collect();
    if decoded
        .iter()
        .all(|fragment| fragment.iter().all(Result::is_ok))
    {
        return Normalized {
            fragments: decoded
                .into_iter()
                .map(|fragment| fragment.into_iter().map(Result::unwrap).collect())
                .collect(),
            replacements: Vec::new(),
        };
    }
    let provisional: Vec<String> = decoded
        .iter()
        .map(|fragment| {
            fragment
                .iter()
                .map(|value| value.unwrap_or('\u{fffd}'))
                .collect()
        })
        .collect();
    let mut observations = Vec::new();
    for fragment in &provisional {
        observations.push(fragment.clone());
        observations.push(unescape(fragment));
    }
    let marker = missing_word(&observations);
    let mut replacements = BTreeMap::new();
    let fragments = decoded
        .into_iter()
        .map(|fragment| {
            let mut output = String::new();
            for value in fragment {
                match value {
                    Ok(character) => output.push(character),
                    Err(unit) => {
                        let text = format!("{marker}{unit:04x}{marker}");
                        output.push_str(&text);
                        replacements.insert(text, unit);
                    }
                }
            }
            output
        })
        .collect();
    Normalized {
        fragments,
        replacements: replacements.into_iter().collect(),
    }
}

// Only escape effects which can create or join the marker alphabet matter.
// Identity escapes keep their following character; line continuations vanish.
fn unescape(source: &str) -> String {
    let characters: Vec<char> = source.chars().collect();
    let mut result = String::new();
    let mut i = 0;
    while i < characters.len() {
        let character = characters[i];
        i += 1;
        if character != '\\' || i == characters.len() {
            result.push(character);
            continue;
        }
        let escaped = characters[i];
        i += 1;
        match escaped {
            '\n' | '\u{2028}' | '\u{2029}' => {}
            '\r' => {
                if characters.get(i) == Some(&'\n') {
                    i += 1;
                }
            }
            'u' => {
                let start = i;
                let mut value = 0u32;
                let mut digits = 0;
                let mut valid = true;
                if characters.get(i) == Some(&'{') {
                    i += 1;
                    while let Some(character) = characters.get(i) {
                        if *character == '}' {
                            break;
                        }
                        if let Some(digit) = character.to_digit(16) {
                            value = value.saturating_mul(16).saturating_add(digit);
                            digits += 1;
                            i += 1;
                        } else {
                            valid = false;
                            break;
                        }
                    }
                    if characters.get(i) == Some(&'}') {
                        i += 1;
                    } else {
                        valid = false;
                    }
                } else {
                    for _ in 0..4 {
                        if let Some(digit) = characters
                            .get(i)
                            .and_then(|character| character.to_digit(16))
                        {
                            value = value * 16 + digit;
                            digits += 1;
                            i += 1;
                        } else {
                            valid = false;
                            break;
                        }
                    }
                }
                if valid && digits > 0 {
                    result.push(char::from_u32(value).unwrap_or('\u{fffd}'));
                } else {
                    i = start;
                    result.push('u');
                }
            }
            // These escape forms always introduce a non-marker character.
            'x' | '0'..='7' | 'b' | 'f' | 'n' | 'r' | 't' | 'v' => result.push('\u{fffd}'),
            character => result.push(character),
        }
    }
    result
}

fn missing_word(observations: &[String]) -> String {
    for length in 1..=usize::BITS as usize {
        let mut words = HashSet::new();
        for source in observations {
            let mut value = 0usize;
            let mut run = 0usize;
            for character in source.chars() {
                let bit = match character {
                    '\u{e000}' => 0,
                    '\u{e001}' => 1,
                    _ => {
                        run = 0;
                        value = 0;
                        continue;
                    }
                };
                value = value.wrapping_shl(1) | bit;
                run += 1;
                if length < usize::BITS as usize {
                    value &= (1usize << length) - 1;
                }
                if run >= length {
                    words.insert(value);
                }
            }
        }
        // At most source length words exist, so a missing word is guaranteed
        // before an unrepresentable candidate bound can become necessary.
        let mut candidate = 0;
        while words.contains(&candidate) {
            candidate += 1;
        }
        if length == usize::BITS as usize || candidate < (1usize << length) {
            return (0..length)
                .rev()
                .map(|bit| {
                    if (candidate >> bit) & 1 == 0 {
                        '\u{e000}'
                    } else {
                        '\u{e001}'
                    }
                })
                .collect();
        }
    }
    unreachable!("a finite source cannot contain every finite marker word")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn restore(source: &str, map: &[(String, u16)]) -> Vec<u16> {
        let mut output = Vec::new();
        let mut remaining = source;
        while !remaining.is_empty() {
            if let Some((word, unit)) = map.iter().find(|(word, _)| remaining.starts_with(word)) {
                output.push(*unit);
                remaining = &remaining[word.len()..];
            } else {
                let character = remaining.chars().next().unwrap();
                output.extend(character.encode_utf16(&mut [0; 2]).iter().copied());
                remaining = &remaining[character.len_utf8()..];
            }
        }
        output
    }
    #[test]
    fn roundtrips_raw_units_and_preserves_real_private_characters() {
        let mut units = "'\u{e000}\\uE001 ".encode_utf16().collect::<Vec<_>>();
        units.extend([0xd800, 0xdc00, 0xd800, 0x27]);
        let result = normalize(&[units.clone()]);
        assert_eq!(restore(&result.fragments[0], &result.replacements), units);
        assert_eq!(result.replacements.len(), 1);
        assert!(!result.replacements[0].0.starts_with("\u{e000}d800"));
    }
    #[test]
    fn escapes_and_continuations_cannot_forge_transport_markers() {
        let original = "'\\uE000\\\n\\u{e001}\\\r\n\\\u{e000}'";
        let mut units = original.encode_utf16().collect::<Vec<_>>();
        units.extend([0xdfff]);
        let result = normalize(&[units.clone()]);
        assert_eq!(restore(&result.fragments[0], &result.replacements), units);
        let marker = &result.replacements[0].0;
        assert!(!unescape(original).contains(marker));
    }
    #[test]
    fn many_fragments_share_one_unforgeable_alphabet() {
        let inputs = vec![
            vec![0xd800],
            "'\u{e000}d800\u{e000}'".encode_utf16().collect(),
            vec![0xdfff],
        ];
        let result = normalize(&inputs);
        for (input, output) in inputs.iter().zip(&result.fragments) {
            assert_eq!(*input, restore(output, &result.replacements));
        }
    }
}
