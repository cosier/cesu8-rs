// Copyright 2012-2014 The Rust Project Developers and Eric Kidd.  See the
// COPYRIGHT-RUST.txt file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed except
// according to those terms.


//! A simple library implementing the [CESU-8 compatibility encoding
//! scheme](http://www.unicode.org/reports/tr26/tr26-2.html).  This is a
//! non-standard variant of UTF-8 that is used internally by some systems
//! that need to represent UTF-16 data as 8-bit characters.  Yes, this is
//! ugly.
//!
//! Use of this encoding is discouraged by the Unicode Consortium.  It's OK
//! for working with existing internal APIs, but it should not be used for
//! transmitting or storing data.
//!
//! ```
//! use std::borrow::Cow;
//! use cesu8::{from_cesu8, to_cesu8};
//!
//! // 16-bit Unicode characters are the same in UTF-8 and CESU-8.
//! assert_eq!(Cow::Borrowed("aé日".as_bytes()),
//!            to_cesu8("aé日"));
//! assert_eq!(Cow::Borrowed("aé日"),
//!            from_cesu8("aé日".as_bytes()).unwrap());
//!
//! // This string is CESU-8 data containing a 6-byte surrogate pair,
//! // which decodes to a 4-byte UTF-8 string.
//! let data = &[0xED, 0xA0, 0x81, 0xED, 0xB0, 0x81];
//! assert_eq!(Cow::Borrowed("\u{10401}"),
//!            from_cesu8(data).unwrap());
//! ```
//!
//! ### A note about security
//!
//! As a general rule, this library is intended to fail on malformed or
//! unexpected input.  CESU-8 is supposed to be an internal-only format,
//! and if we're seeing malformed data, we assume that it's either a bug in
//! somebody's code, or an attacker is trying to improperly encode data to
//! evade security checks.
//!
//! If you have a use case for lossy conversion to UTF-8, or conversion
//! from mixed UTF-8/CESU-8 data, please feel free to submit a pull request
//! for `from_cesu8_lossy_permissive` with appropriate behavior.
//!
//! ### Java and U+0000, and other variants
//!
//! Java uses the CESU-8 encoding as described above, but with one
//! difference: The null character U+0000 is represented as an overlong
//! UTF-8 sequence.  This is not currently supported by this library, but
//! pull requests to add `from_java_cesu8` and `to_java_cesu8` are welcome.
//!
//! ### Surrogate pairs and UTF-8
//!
//! The UTF-16 encoding uses "surrogate pairs" to represent Unicode code
//! points in the range from U+10000 to U+10FFFF.  These are 16-bit numbers
//! in the range 0xD800 to 0xDFFF.
//!
//! * 0xD800 to 0xDBFF: First half of surrogate pair.  When encoded as
//!   CESU-8, these become **1110**1101 **10**100000 **10**000000 to
//!   **1110**1101 **10**101111 **10**111111.
//! 
//! * 0xDC00 to 0xDFFF: Second half of surrogate pair.  These become
//!   **1110**1101 **10**110000 **10**000000 to
//!   **1110**1101 **10**111111 **10**111111.
//!
//! Wikipedia [explains](http://en.wikipedia.org/wiki/UTF-16) the
//! code point to UTF-16 conversion process:
//!
//! > Consider the encoding of U+10437 (𐐷):
//! > 
//! > * Subtract 0x10000 from 0x10437. The result is 0x00437, 0000 0000 0100
//! >   0011 0111.
//! > * Split this into the high 10-bit value and the low 10-bit value:
//! >   0000000001 and 0000110111.
//! > * Add 0xD800 to the high value to form the high surrogate: 0xD800 +
//! >   0x0001 = 0xD801.
//! > * Add 0xDC00 to the low value to form the low surrogate: 0xDC00 +
//! >   0x0037 = 0xDC37.

#![warn(missing_docs)]

#![feature(str_utf16)]

use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::result::Result;
use std::slice;
use std::str::{from_utf8, from_utf8_unchecked};
use unicode::utf8_char_width;

mod unicode;

/// Mask of the value bits of a continuation byte.
const CONT_MASK: u8 = 0b0011_1111u8;
/// Value of the tag bits (tag mask is !CONT_MASK) of a continuation byte.
const TAG_CONT_U8: u8 = 0b1000_0000u8;

/// The CESU-8 data could not be decoded as valid UTF-8 data.
#[derive(Clone, Copy, Debug)]
pub struct Cesu8DecodingError;

impl Error for Cesu8DecodingError {
    fn description(&self) -> &str { "decoding error" }
    fn cause(&self) -> Option<&Error> { None }
}

impl fmt::Display for Cesu8DecodingError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "could not convert CESU-8 data to UTF-8")
    }
}

/// Convert CESU-8 data to a Rust string, re-encoding only if necessary.
/// Returns an error if the data cannot be represented as valid UTF-8.
///
/// ```
/// use std::borrow::Cow;
/// use cesu8::from_cesu8;
///
/// // This string is valid as UTF-8 or CESU-8, so it doesn't change,
/// // and we can convert it without allocating memory.
/// assert_eq!(Cow::Borrowed("aé日"),
///            from_cesu8("aé日".as_bytes()).unwrap());
///
/// // This string is CESU-8 data containing a 6-byte surrogate pair,
/// // which becomes a 4-byte UTF-8 string.
/// let data = &[0xED, 0xA0, 0x81, 0xED, 0xB0, 0x81];
/// assert_eq!(Cow::Borrowed("\u{10401}"),
///            from_cesu8(data).unwrap());
/// ```
pub fn from_cesu8(bytes: &[u8]) -> Result<Cow<str>, Cesu8DecodingError> {
    match from_utf8(bytes) {
        Ok(str) => Ok(Cow::Borrowed(str)),
        _ => {
            let mut decoded = Vec::with_capacity(bytes.len());
            if decode_from_iter(&mut decoded, &mut bytes.iter()) {
                // We can remove this assertion if we trust our decoder.
                assert!(from_utf8(&decoded[..]).is_ok());
                Ok(Cow::Owned(unsafe { String::from_utf8_unchecked(decoded) }))
            } else {
                Err(Cesu8DecodingError)
            }
        }
    }    
}

#[test]
fn test_from_cesu8() {
    // The surrogate-encoded character below is from the ICU library's
    // icu/source/test/testdata/conversion.txt test case.
    let data = &[0x4D, 0xE6, 0x97, 0xA5, 0xED, 0xA0, 0x81, 0xED, 0xB0, 0x81];
    assert_eq!(Cow::Borrowed("M日\u{10401}"),
               from_cesu8(data).unwrap());

    // We used to have test data from the CESU-8 specification, but when we
    // worked it through manually, we got the wrong answer:
    // 
    // Input: [0xED, 0xAE, 0x80, 0xED, 0xB0, 0x80]
    // Binary: 11101101 10101110 10000000 11101101 10110000 10000000
    // 
    // 0b1101_101110_000000 -> 0xDB80
    // 0b1101_110000_000000 -> 0xDC00
    // 
    // ((0xDB80 - 0xD800) << 10) | (0xDC00 - 0xDC00) -> 0xE0000
    // 0x10000 + 0xE0000 -> 0xF0000
    // 
    // The spec claims that we are supposed to get 0x10000, not 0xF0000.
    // Since I can't reconcile this example data with the text of the
    // specification, I decided to use a test character from ICU instead.
}

// Our internal decoder, based on Rust's is_utf8 implementation.
fn decode_from_iter(decoded: &mut Vec<u8>, iter: &mut slice::Iter<u8>) -> bool {
    macro_rules! err {
        () => { return false }
    }
    macro_rules! next {
        () => {
            match iter.next() {
                Some(a) => *a,
                // We needed data, but there was none: error!
                None => err!()
            }
        }
    }
    macro_rules! next_cont {
        () => {
            {
                let byte = next!();
                if (byte) & !CONT_MASK == TAG_CONT_U8 { byte } else { err!() }
            }
        }
    }

    loop {
        let first = match iter.next() {
            Some(&b) => b,
            // We're at the end of the iterator and a codepoint boundary at
            // the same time, so this string is valid.
            None => return true
        };

        if first < 127 {
            // Pass ASCII through directly.
            decoded.push(first);
        } else {
            let w = utf8_char_width(first);
            let second = next_cont!();
            match w {
                // Two-byte sequences can be used directly.
                2 => { decoded.extend([first, second].iter().cloned()); }
                3 => {
                    let third = next_cont!();
                    match (first, second) {
                        // These are valid UTF-8, so pass them through.
                        (0xE0         , 0xA0 ... 0xBF) |
                        (0xE1 ... 0xEC, 0x80 ... 0xBF) |
                        (0xED         , 0x80 ... 0x9F) |
                        (0xEE ... 0xEF, 0x80 ... 0xBF) => {
                            decoded.extend([first, second, third].iter()
                                               .cloned())
                        }
                        // First half a surrogate pair, so decode.
                        (0xED         , 0xA0 ... 0xAF) => {
                            if next!() != 0xED { err!() }
                            let fifth = next_cont!();
                            if fifth < 0xB0 || 0xBF < fifth { err!() }
                            let sixth = next_cont!();
                            let s = dec_surrogates(second, third, fifth, sixth);
                            decoded.extend(s.iter().cloned());
                        }
                        _ => err!()
                    }
                }
                _ => err!()
            }
        }
    }
}

/// Convert the two trailing bytes from a CESU-8 surrogate to a regular
/// surrogate value.
fn dec_surrogate(second: u8, third: u8) -> u32 {
    0xD000u32 | ((second & CONT_MASK) as u32) << 6 | (third & CONT_MASK) as u32
}

/// Convert the bytes from a CESU-8 surrogate pair into a valid UTF-8
/// sequence.  Assumes input is valid.
fn dec_surrogates(second: u8, third: u8, fifth: u8, sixth: u8) -> [u8; 4] {
    // Convert to a 32-bit code point.
    let s1 = dec_surrogate(second, third);
    let s2 = dec_surrogate(fifth, sixth);
    let c = 0x10000 + (((s1 - 0xD800) << 10) | (s2 - 0xDC00));
    //println!("{:0>8b} {:0>8b} {:0>8b} -> {:0>16b}", 0xEDu8, second, third, s1);
    //println!("{:0>8b} {:0>8b} {:0>8b} -> {:0>16b}", 0xEDu8, fifth, sixth, s2);
    //println!("-> {:0>32b}", c);
    assert!(0x010000 <= c && c <= 0x10FFFF);

    // Convert to UTF-8.
    // 11110xxx 10xxxxxx 10xxxxxx 10xxxxxx 
    [0b1111_0000u8 | ((c & 0b1_1100_0000_0000_0000_0000) >> 18) as u8,
     TAG_CONT_U8   | ((c & 0b0_0011_1111_0000_0000_0000) >> 12) as u8,
     TAG_CONT_U8   | ((c & 0b0_0000_0000_1111_1100_0000) >>  6) as u8,
     TAG_CONT_U8   | ((c & 0b0_0000_0000_0000_0011_1111)      ) as u8]
}

/// Convert a Rust `&str` to CESU-8 bytes.
///
/// ```
/// use std::borrow::Cow;
/// use cesu8::to_cesu8;
///
/// // This string is valid as UTF-8 or CESU-8, so it doesn't change,
/// // and we can convert it without allocating memory.
/// assert_eq!(Cow::Borrowed("aé日".as_bytes()), to_cesu8("aé日"));
///
/// // This string is a 4-byte UTF-8 string, which becomes a 6-byte CESU-8
/// // vector.
/// assert_eq!(Cow::Borrowed(&[0xED, 0xA0, 0x81, 0xED, 0xB0, 0x81]),
///            to_cesu8("\u{10401}"));
/// ```
pub fn to_cesu8(text: &str) -> Cow<[u8]> {
    if is_valid_cesu8(text) {
        Cow::Borrowed(text.as_bytes())
    } else {
        let bytes = text.as_bytes();
        let mut encoded = Vec::with_capacity(bytes.len() + bytes.len() >> 2);
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b < 128 {
                // Pass ASCII through quickly.
                encoded.push(b);
                i += 1;
            } else {
                // Figure out how many bytes we need for this character.
                let w = utf8_char_width(b);
                assert!(w <= 4);
                assert!(i + w <= bytes.len());
                if w != 4 {
                    // Pass through short UTF-8 sequences unmodified.
                    encoded.extend(bytes[i..i+w].iter().cloned());
                } else {
                    // Encode 4-byte sequences as 6 bytes.
                    let s = unsafe { from_utf8_unchecked(&bytes[i..i+w]) };
                    for u in s.utf16_units() {
                        encoded.extend(enc_surrogate(u).iter().cloned());
                    }
                }
                i += w;
            }
        }
        Cow::Owned(encoded)
    }
}

/// Check whether a Rust string contains valid CESU-8 data.
pub fn is_valid_cesu8(text: &str) -> bool {
    // We rely on the fact that Rust strings are guaranteed to be valid
    // UTF-8.
    for b in text.bytes() {
        if (b & !CONT_MASK) == TAG_CONT_U8 { continue; }
        if utf8_char_width(b) > 3 { return false; }
    }
    true
}

/// Encode a single surrogate as CESU-8.
fn enc_surrogate(surrogate: u16) -> [u8; 3] {
    assert!(0xD800 <= surrogate && surrogate <= 0xDFFF);
    // 1110xxxx 10xxxxxx 10xxxxxx
    [0b11100000  | ((surrogate & 0b11110000_00000000) >> 12) as u8,
     TAG_CONT_U8 | ((surrogate & 0b00001111_11000000) >>  6) as u8,
     TAG_CONT_U8 | ((surrogate & 0b00000000_00111111)      ) as u8]
}
