//! Turning a name a caller asked for into the entries that store it.
//!
//! Reading a long name is nearly mechanical. Writing one is not, because a
//! long name never travels alone: every long name also gets an 8.3 alias,
//! that alias has to be unique within its directory, and the two are bound
//! together by a checksum over the alias's stored bytes. Get the alias
//! wrong and the long name is orphaned; get the uniqueness wrong and two
//! files answer to one name.
//!
//! # Three classes of character, not one
//!
//! A character can be **rejected**, **replaced** or **skipped**, and
//! collapsing them loses information:
//!
//! * Rejected — the control characters and `* ? < > | " : / \` — are
//!   illegal in a long name too, so the name is refused outright.
//! * Replaced — `[ ] ; , + =` — are perfectly legal in a long name and
//!   merely have no place in an alias, so the alias gets `_` and the long
//!   name is stored intact.
//! * Skipped — `.` and space — are dropped from the alias entirely.
//!
//! The last two also mean the name is not representable as 8.3, which is
//! what forces long-name slots to be written.
//!
//! # When no slots are written at all
//!
//! A name that already fits 8.3 gets **no long-name slots**, even when its
//! case differs from the stored form: two bits in the entry say "show the
//! base lower case" and "show the extension lower case". So `game.nes`
//! costs one directory entry and `Game.nes` costs three. In a directory of
//! several hundred ROMs that is a direct saving in both the size of the
//! directory and the time to enumerate it, which is the case this crate
//! exists for.

use alloc::vec::Vec;

use crate::codepage::Codepage;
use crate::dir::{Attributes, ENTRY_SIZE, ShortName, is_valid_short_name_byte, lcase};
use crate::error::{Error, Result};

/// The most UTF-16 units a long name may hold.
const MAX_LONG_NAME: usize = 255;

/// Characters carried by one long-name slot.
const CHARS_PER_SLOT: usize = 13;

/// The highest numeric tail probed before an alias is given up on.
///
/// Seven characters of `~999999` leave one for the base, which is the point
/// at which a longer tail would have nothing left to disambiguate. It is
/// also far beyond reach: an aliased name costs at least two directory
/// entries, and a FAT directory holds 65536, so no directory can contain
/// even 32768 names competing for one basis — let alone a million.
const MAX_TAIL: u32 = 999_999;

/// A name as it will be stored: the 8.3 entry, and the slots that precede
/// it.
///
/// `slots` is in **on-disk order**, which is the reverse of reading order:
/// the last slot of the name comes first and carries the end marker in its
/// sequence byte, and the entry named by [`short`](Self::short) follows all
/// of them. It is empty when the name fits 8.3 on its own.
pub(crate) struct StoredName {
    /// The 8.3 entry's name, with its case bits.
    pub short: ShortName,
    /// The long-name slots, ready to write.
    pub slots: Vec<[u8; ENTRY_SIZE]>,
}

impl StoredName {
    /// How many directory slots storing this name takes.
    pub fn entries(&self) -> u32 {
        self.slots.len() as u32 + 1
    }
}

/// Works out how `name` should be stored in a directory.
///
/// `taken` is asked whether a candidate 8.3 alias is already used, and is
/// the reason this function needs a directory at all. With the directory
/// parsed and resident, that question is a map lookup — which is what makes
/// probing for a free alias affordable enough to be simple: there is no
/// need for the hash-based shortcut a driver that would have to rescan the
/// directory for every probe is forced into.
pub(crate) fn stored_name<E>(
    name: &str,
    codepage: &Codepage,
    taken: &mut dyn FnMut(&str) -> bool,
) -> Result<StoredName, E> {
    let bad = || Error::BadName {
        name: alloc::string::String::from(name),
    };
    let units = long_name_units(name).ok_or_else(bad)?;

    // A name that fits 8.3 as it stands. Two outcomes: identical case
    // throughout each field, which the case bits can express and which
    // therefore needs no slots at all; or mixed case, which needs slots but
    // whose alias is still the name itself rather than a mangled one.
    if let Some(direct) = direct_8_3(name, codepage) {
        if let Some(flags) = direct.case_flags() {
            return Ok(StoredName {
                short: ShortName::from_raw(direct.raw, flags),
                slots: Vec::new(),
            });
        }

        // Mixed case. The alias is exact, so use it — unless something else
        // already has it, which is possible without the two names colliding:
        // another entry's own alias may have been mangled into this one.
        let short = ShortName::from_raw(direct.raw, 0);
        if !taken(&short.to_display_string_with(codepage)) {
            let slots = long_name_slots(&units, short.checksum());
            return Ok(StoredName { short, slots });
        }
    }

    // Otherwise the name has to be mangled, and a mangled alias always
    // carries a numeric tail even when it happens to be free. That is what
    // every other implementation does, and matching it matters: an alias is
    // a name users see and scripts hard-code.
    let (base, extension) = basis_name(name, codepage);
    for tail in 1..=MAX_TAIL {
        let short = ShortName::from_raw(with_tail(&base, &extension, tail), 0);
        if !taken(&short.to_display_string_with(codepage)) {
            let slots = long_name_slots(&units, short.checksum());
            return Ok(StoredName { short, slots });
        }
    }
    Err(Error::NoAliasAvailable {
        name: alloc::string::String::from(name),
    })
}

/// The UTF-16 units of a name a long-name entry could hold, or `None`.
///
/// The refusals are the ones no FAT implementation would store either: an
/// empty name, one longer than the format's 255 units, the two names a
/// directory already uses for itself, a reserved character, and the leading
/// or trailing spaces and trailing periods that Windows silently strips —
/// silently stripping them here would mean handing back a file under a
/// different name than the one asked for.
fn long_name_units(name: &str) -> Option<Vec<u16>> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with(' ')
        || name.ends_with(' ')
        || name.ends_with('.')
        || name.chars().any(is_reserved)
    {
        return None;
    }
    let units: Vec<u16> = name.encode_utf16().collect();
    (units.len() <= MAX_LONG_NAME).then_some(units)
}

/// Characters no FAT name may contain, long or short.
fn is_reserved(character: char) -> bool {
    matches!(
        character,
        '"' | '*' | '/' | ':' | '<' | '>' | '?' | '\\' | '|'
    ) || (character as u32) < 0x20
}

/// The 8.3 name that stores `name` exactly, if one does.
///
/// "Exactly" means the stored form can be turned back into the string that
/// was asked for: nothing replaced, nothing skipped, and a case the two case
/// bits can express. A name that only *nearly* fits comes back as `None`,
/// because the alternative is handing back a name the caller did not ask
/// for.
pub(crate) fn exact_8_3(name: &str, codepage: &Codepage) -> Option<ShortName> {
    // Checked for its refusals, not for its result: a name with no storable
    // form at all has no exact 8.3 form either.
    long_name_units(name)?;
    let direct = direct_8_3(name, codepage)?;
    Some(ShortName::from_raw(direct.raw, direct.case_flags()?))
}

/// A name that fits 8.3 without being mangled.
struct Direct {
    /// The eleven stored bytes, upper cased and space padded.
    raw: [u8; 11],
    /// `Some(true)` if the base is entirely lower case, `Some(false)` if it
    /// is entirely upper case or has no case at all, and `None` if it mixes
    /// the two — which one bit cannot express.
    base: Option<bool>,
    /// The same, for the extension.
    extension: Option<bool>,
}

impl Direct {
    /// The case bits for this name, or `None` if no pair of bits can say
    /// what its case is — which is what forces long-name slots.
    fn case_flags(&self) -> Option<u8> {
        let mut flags = 0;
        if self.base? {
            flags |= lcase::BASE;
        }
        if self.extension? {
            flags |= lcase::EXTENSION;
        }
        Some(flags)
    }
}

/// Fits `name` into 8.3 without losing anything, or gives up.
///
/// Gives up on the first character that would have to be replaced, skipped
/// or dropped, because "fits" here means the stored name can be turned back
/// into exactly what was asked for.
fn direct_8_3(name: &str, codepage: &Codepage) -> Option<Direct> {
    let (base, extension) = match name.rsplit_once('.') {
        Some(split) => split,
        None => (name, ""),
    };
    if base.is_empty() || base.chars().count() > 8 || extension.chars().count() > 3 {
        return None;
    }

    let mut raw = [b' '; 11];
    for (field, at) in [(base, 0usize), (extension, 8usize)] {
        for (n, character) in field.chars().enumerate() {
            let byte = codepage.encode(upper(character)?)?;
            if !is_valid_short_name_byte(byte) {
                return None;
            }
            raw[at + n] = byte;
        }
    }

    Some(Direct {
        raw,
        base: field_case(base),
        extension: field_case(extension),
    })
}

/// Whether a field is uniformly cased, and which way.
fn field_case(field: &str) -> Option<bool> {
    let lower = field.chars().any(char::is_lowercase);
    let upper = field.chars().any(char::is_uppercase);
    match (lower, upper) {
        (true, true) => None,
        (true, false) => Some(true),
        _ => Some(false),
    }
}

/// The upper-case form of a character, if one character is enough.
///
/// German `ß` upper-cases to `SS`, and a few other characters lengthen the
/// same way. Rather than growing a name behind the caller's back, those are
/// treated as not storable in an alias, which sends them down the mangling
/// path where they become `_`.
fn upper(character: char) -> Option<char> {
    let mut characters = character.to_uppercase();
    let first = characters.next()?;
    characters.next().is_none().then_some(first)
}

/// Mangles a name into the base and extension a numeric tail is added to.
///
/// Leading periods and spaces go first — `.gitignore` has an eight-letter
/// base, not an empty one — then each field keeps the characters it can and
/// substitutes `_` for the rest.
/// The base is never empty by the time this returns, and does not need a
/// fallback: stripping leaves a first character that is neither a period nor
/// a space, so it survives mangling as itself or as `_`. A name consisting
/// of nothing but periods and spaces never reaches here — it either ends
/// with one, or is `.` or `..`, and [`long_name_units`] refuses all three.
fn basis_name(name: &str, codepage: &Codepage) -> (Vec<u8>, Vec<u8>) {
    let stripped = name.trim_start_matches(['.', ' ']);
    let (base, extension) = match stripped.rsplit_once('.') {
        Some(split) => split,
        None => (stripped, ""),
    };
    (mangle(base, 8, codepage), mangle(extension, 3, codepage))
}

/// Maps one field into at most `limit` short-name bytes.
fn mangle(field: &str, limit: usize, codepage: &Codepage) -> Vec<u8> {
    let mut out = Vec::with_capacity(limit);
    for character in field.chars() {
        if out.len() == limit {
            break;
        }
        if character == '.' || character == ' ' {
            continue;
        }
        let byte = upper(character)
            .and_then(|upper| codepage.encode(upper))
            .filter(|&byte| is_valid_short_name_byte(byte));
        out.push(byte.unwrap_or(b'_'));
    }
    out
}

/// Builds `BASE~n.EXT`, trimming the base to leave room for the tail.
fn with_tail(base: &[u8], extension: &[u8], tail: u32) -> [u8; 11] {
    debug_assert!((1..=MAX_TAIL).contains(&tail));

    let mut digits = [0u8; 6];
    let mut length = 0;
    let mut left = tail;
    loop {
        digits[length] = b'0' + (left % 10) as u8;
        length += 1;
        left /= 10;
        if left == 0 {
            break;
        }
    }

    // The tail is the `~` plus the digits, and it always wins: an alias
    // that kept the base and dropped a digit would not be unique, which is
    // the tail's only job.
    let keep = base.len().min(8 - (length + 1));
    let mut raw = [b' '; 11];
    raw[..keep].copy_from_slice(&base[..keep]);
    raw[keep] = b'~';
    for (n, digit) in digits[..length].iter().rev().enumerate() {
        raw[keep + 1 + n] = *digit;
    }
    raw[8..8 + extension.len()].copy_from_slice(extension);
    raw
}

/// Builds the long-name slots for `units`, in the order they are stored.
///
/// On disk a long name runs backwards: the slot holding the *last*
/// thirteen characters comes first and has bit 6 of its sequence byte set,
/// and the 8.3 entry follows the whole run. That way an implementation
/// reading forwards meets the end marker before the characters, and knows
/// how many slots to expect before it has to buffer any of them.
fn long_name_slots(units: &[u16], checksum: u8) -> Vec<[u8; ENTRY_SIZE]> {
    let count = units.len().div_ceil(CHARS_PER_SLOT);
    let mut slots = Vec::with_capacity(count);

    for sequence in (1..=count).rev() {
        let mut slot = [0u8; ENTRY_SIZE];
        slot[0] = sequence as u8 | if sequence == count { 0x40 } else { 0 };
        slot[0x0B] = Attributes::LONG_NAME.bits();
        slot[0x0D] = checksum;
        // Bytes 0x1A..0x1C are the first-cluster field of an ordinary
        // entry, and must be zero here. They already are.

        let from = (sequence - 1) * CHARS_PER_SLOT;
        for n in 0..CHARS_PER_SLOT {
            let unit = match units.get(from + n) {
                Some(&unit) => unit,
                // One NUL terminates the name, and 0xFFFF pads out the
                // rest. A name that exactly fills its last slot gets
                // neither, which is why a reader cannot rely on finding a
                // terminator.
                None if from + n == units.len() => 0x0000,
                None => 0xFFFF,
            };
            let at = char_offset(n);
            slot[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        slots.push(slot);
    }
    slots
}

/// Where the `n`th character of a long-name slot lives.
///
/// Three runs rather than one, because the slot has to leave the attribute,
/// type, checksum and first-cluster fields where an implementation that
/// does not understand long names expects to find them.
fn char_offset(n: usize) -> usize {
    match n {
        0..=4 => 0x01 + n * 2,
        5..=10 => 0x0E + (n - 5) * 2,
        _ => 0x1C + (n - 11) * 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;

    /// Builds a name in a directory that already holds `existing`.
    fn build(name: &str, existing: &[&str]) -> Result<StoredName, ()> {
        let owned: Vec<String> = existing.iter().map(|n| n.to_uppercase()).collect();
        stored_name(name, &Codepage::ASCII, &mut |alias| {
            owned.iter().any(|taken| taken == &alias.to_uppercase())
        })
    }

    fn alias(stored: &StoredName) -> String {
        stored.short.to_display_string()
    }

    /// A name that already fits 8.3 costs one entry, whatever its case.
    #[test]
    fn a_short_name_needs_no_slots() {
        for name in ["GAME.NES", "game.nes", "readme", "123.TXT", "a.b"] {
            let stored = build(name, &[]).expect("should fit");
            assert!(
                stored.slots.is_empty(),
                "{name} should need no long-name slots"
            );
            assert_eq!(alias(&stored), name, "{name} should round-trip");
        }
    }

    /// Mixed case is what the case bits cannot express, so that is where
    /// slots start being needed -- and the alias is still the name itself.
    #[test]
    fn mixed_case_needs_slots_but_not_a_tail() {
        let stored = build("ReadMe.txt", &[]).expect("should fit as an alias");
        assert_eq!(alias(&stored), "README.TXT");
        assert_eq!(stored.slots.len(), 1);
    }

    /// A name too long, or carrying characters 8.3 has no room for, is
    /// mangled and given a tail.
    #[test]
    fn names_that_do_not_fit_are_mangled() {
        let cases = [
            ("toolongforeight.txt", "TOOLON~1.TXT"),
            ("has space.txt", "HASSPA~1.TXT"),
            ("two.dots.txt", "TWODOT~1.TXT"),
            ("GOOD.LONGEXT", "GOOD~1.LON"),
            ("plus+equals=.txt", "PLUS_E~1.TXT"),
            (".gitignore", "GITIGN~1"),
        ];
        for (name, expected) in cases {
            let stored = build(name, &[]).expect("should be mangled, not refused");
            assert_eq!(alias(&stored), expected, "{name}");
            assert!(!stored.slots.is_empty(), "{name} needs its long name kept");
        }
    }

    /// Colliding aliases keep counting past the nine Linux stops at, which
    /// the resident directory makes affordable.
    ///
    /// The base gives up a character each time the tail needs another
    /// digit, rather than the tail being truncated -- a shortened tail
    /// would not be unique, which is the only thing it is there for.
    #[test]
    fn a_tail_counts_past_nine() {
        let mut existing: Vec<String> = Vec::new();
        for expected in [
            "ALONGN~1.TXT",
            "ALONGN~2.TXT",
            "ALONGN~3.TXT",
            "ALONGN~4.TXT",
            "ALONGN~5.TXT",
            "ALONGN~6.TXT",
            "ALONGN~7.TXT",
            "ALONGN~8.TXT",
            "ALONGN~9.TXT",
            "ALONG~10.TXT",
            "ALONG~11.TXT",
            "ALONG~12.TXT",
        ] {
            let borrowed: Vec<&str> = existing.iter().map(String::as_str).collect();
            let stored = build("a long name.txt", &borrowed).expect("build");
            assert_eq!(alias(&stored), expected);
            existing.push(alias(&stored));
        }

        // And on into three digits.
        let taken: Vec<String> = (1..=9)
            .map(|n| alloc::format!("ALONGN~{n}.TXT"))
            .chain((10..=99).map(|n| alloc::format!("ALONG~{n}.TXT")))
            .collect();
        let borrowed: Vec<&str> = taken.iter().map(String::as_str).collect();
        let stored = build("a long name.txt", &borrowed).expect("build");
        assert_eq!(alias(&stored), "ALON~100.TXT");
    }

    /// Characters illegal in a long name are refused rather than replaced,
    /// because there is no form of the name left to store.
    #[test]
    fn reserved_characters_are_refused() {
        for name in [
            "bad*char.txt",
            "who?.txt",
            "a<b.txt",
            "pipe|.txt",
            "",
            ".",
            "..",
            "trailing.",
            "trailing ",
            " leading",
        ] {
            assert!(
                build(name, &[]).is_err(),
                "{name:?} should have been refused"
            );
        }
    }

    /// The slots are the reverse of reading order, carry the alias's
    /// checksum, and terminate the name the way the format says.
    #[test]
    fn slots_are_stored_backwards_and_checksummed() {
        // Fourteen characters: one more than a slot holds, so two slots.
        let stored = build("fourteen chars", &[]).expect("build");
        assert_eq!(stored.slots.len(), 2);
        assert_eq!(stored.entries(), 3);

        let checksum = stored.short.checksum();
        assert_eq!(stored.slots[0][0], 0x40 | 2, "the last slot comes first");
        assert_eq!(stored.slots[1][0], 1);
        for slot in &stored.slots {
            assert_eq!(slot[0x0B], Attributes::LONG_NAME.bits());
            assert_eq!(slot[0x0D], checksum);
            assert_eq!(&slot[0x1A..0x1C], &[0, 0], "slots claim no cluster");
        }

        // Reading the characters back out in sequence order gives the name,
        // a NUL, then padding.
        let mut units = vec![];
        for slot in stored.slots.iter().rev() {
            for n in 0..CHARS_PER_SLOT {
                let at = char_offset(n);
                units.push(u16::from_le_bytes([slot[at], slot[at + 1]]));
            }
        }
        assert_eq!(
            String::from_utf16(&units[..14]).expect("utf16"),
            "fourteen chars"
        );
        assert_eq!(units[14], 0x0000, "the name is terminated");
        assert_eq!(&units[15..], &[0xFFFF; 11], "and then padded");
    }

    /// A name that exactly fills its slots has no terminator, which is the
    /// case a reader looking for one gets wrong.
    #[test]
    fn an_exactly_filling_name_has_no_terminator() {
        let stored = build("thirteenchars", &[]).expect("build");
        assert_eq!(stored.slots.len(), 1);
        let slot = &stored.slots[0];
        let last = char_offset(12);
        assert_eq!(
            u16::from_le_bytes([slot[last], slot[last + 1]]),
            u16::from(b's'),
            "the thirteenth character reaches the end of the slot"
        );
    }

    /// The longest name the format holds fits, and one character more does
    /// not.
    #[test]
    fn the_length_limit_is_the_formats() {
        let longest = "x".repeat(MAX_LONG_NAME);
        let stored = build(&longest, &[]).expect("255 characters is the limit");
        assert_eq!(stored.slots.len(), 20);
        assert!(build(&"x".repeat(MAX_LONG_NAME + 1), &[]).is_err());
    }
}
