//! The optional hook for 8.3 names that are not ASCII.
//!
//! Only the short name is stored in an OEM codepage; long names are UCS-2
//! and never need one. That makes this a far smaller surface than a
//! general-purpose driver's character-set layer — two functions, one each
//! way — and it is why the tables are the caller's rather than this crate's.
//!
//! The default, [`Codepage::ASCII`], is the subset every OEM codepage
//! agrees on. It is the right answer for a volume this crate wrote, and for
//! the great majority of volumes it will read: a name outside it decodes to
//! the replacement character rather than to the wrong character, and the
//! raw bytes stay available through [`ShortName::as_bytes`] either way.
//!
//! Supply a codepage when a consumer's cards really do carry, say, CP437 or
//! CP1252 names. Nothing else changes: the bytes stored are the same, and
//! so is the checksum binding a long name to its entry, which is computed
//! over bytes and never over characters.
//!
//! [`ShortName::as_bytes`]: crate::dir::ShortName::as_bytes

/// Translates between a byte in an 8.3 name and the character it stands
/// for.
///
/// Function pointers rather than a trait, so this is a plain value a
/// consumer passes at mount: no type parameter spreading through
/// [`FileSystem`](crate::FileSystem), and no lifetime. A codepage is a pair
/// of pure lookups, and nothing about it needs state.
#[derive(Debug, Clone, Copy)]
pub struct Codepage {
    decode: fn(u8) -> Option<char>,
    encode: fn(char) -> Option<u8>,
}

impl Codepage {
    /// The ASCII subset, and the default.
    ///
    /// Bytes below 0x80 stand for themselves; anything above has no meaning
    /// without knowing which codepage a volume was written with, so it is
    /// declined rather than guessed at.
    pub const ASCII: Codepage = Codepage {
        decode: decode_ascii,
        encode: encode_ascii,
    };

    /// A codepage from a pair of lookups.
    ///
    /// `decode` maps a stored byte to the character it represents, and
    /// `encode` maps a character to the byte that would store it. Both
    /// return `None` for anything the codepage does not cover: an
    /// undecodable byte is shown as the replacement character, and an
    /// unencodable character forces a name to be stored as a long name with
    /// a mangled 8.3 alias.
    ///
    /// The two should agree with each other. They are not checked against
    /// one another, because a codepage is free to decode a byte it would
    /// never produce — several map two bytes to one character.
    pub const fn new(decode: fn(u8) -> Option<char>, encode: fn(char) -> Option<u8>) -> Self {
        Codepage { decode, encode }
    }

    /// The character a stored byte stands for, if this codepage has one.
    pub fn decode(&self, byte: u8) -> Option<char> {
        (self.decode)(byte)
    }

    /// The byte that would store `character`, if this codepage has one.
    pub fn encode(&self, character: char) -> Option<u8> {
        (self.encode)(character)
    }
}

impl Default for Codepage {
    /// [`Codepage::ASCII`], the subset every OEM codepage agrees on.
    fn default() -> Self {
        Codepage::ASCII
    }
}

fn decode_ascii(byte: u8) -> Option<char> {
    if byte < 0x80 {
        Some(byte as char)
    } else {
        None
    }
}

fn encode_ascii(character: char) -> Option<u8> {
    if character.is_ascii() {
        Some(character as u8)
    } else {
        None
    }
}
