//! A minimal, independent USB HID report-descriptor walker for the BLE HID profiles.
//!
//! This exists because a malformed report map is invisible from inside NetGet: the base BLE
//! stack treats a report map as an opaque byte string, hands it to the central on request,
//! and never parses it. The only thing that ever validates it is the host's HID parser, on
//! someone else's machine, after the descriptor has already shipped. Four of this repo's
//! four HID profiles were shipping descriptors a host would reject, and nothing failed.
//!
//! It is written from the item encoding in the USB HID specification 1.11 section 6.2.2
//! rather than by calling any of NetGet's own code, so it is an independent reading of the
//! spec in the sense `CLAUDE.md` means — not an independent implementation, but enough to
//! catch a mis-sized item, a reserved tag, an unbalanced collection or a report that is not
//! a whole number of bytes.
//!
//! It lives in the keyboard profile's test directory and is `#[path]`-included by the mouse,
//! gamepad and remote suites. That keeps it inside the BLE HID profile boundary: putting it
//! in `tests/helpers/` would mean editing `tests/helpers/mod.rs`, which every protocol in the
//! tree shares.

#![allow(dead_code)]

/// What a successful walk found.
#[derive(Debug, Default)]
pub struct Walked {
    /// Total bits contributed by every `Input` main item, in declaration order.
    pub input_bits: usize,
    /// The explicit `Usage` values in scope at each `Input` item, in declaration order.
    /// A `Usage Minimum`/`Usage Maximum` range contributes its expansion.
    pub input_usages: Vec<Vec<u32>>,
}

impl Walked {
    /// Byte length of the input report, or an error if it is not a whole number of bytes.
    ///
    /// A report that is not byte aligned is the defect this catches most often: appending a
    /// one-bit pad to an already-aligned report costs a whole extra byte on the wire and
    /// silently disagrees with every fixed-length value the profile publishes.
    pub fn input_report_len(&self) -> Result<usize, String> {
        if self.input_bits % 8 != 0 {
            return Err(format!(
                "input report is {} bits, which is not a whole number of bytes; a host pads \
                 to {} bytes and every published fixed-length value is then wrong",
                self.input_bits,
                self.input_bits.div_ceil(8)
            ));
        }
        Ok(self.input_bits / 8)
    }
}

/// Walk a report descriptor, returning `Err` with a byte offset on the first malformed item.
pub fn walk(desc: &[u8]) -> Result<Walked, String> {
    let mut out = Walked::default();
    let (mut size, mut count): (Option<usize>, Option<usize>) = (None, None);
    let mut usage_page: u32 = 0;
    let mut pending: Vec<u32> = Vec::new();
    let mut usage_min: Option<u32> = None;
    let mut depth: i32 = 0;
    let mut i = 0usize;

    while i < desc.len() {
        let prefix = desc[i];
        // 6.2.2.2: bSize of 3 means four data bytes, not three.
        let n = match prefix & 0x03 {
            3 => 4,
            b => b as usize,
        };
        if i + 1 + n > desc.len() {
            return Err(format!(
                "offset {}: item 0x{:02x} declares {} data bytes but only {} remain (the \
                 descriptor is truncated mid-item)",
                i,
                prefix,
                n,
                desc.len() - i - 1
            ));
        }
        let data = &desc[i + 1..i + 1 + n];
        let mut val: u32 = 0;
        for (k, b) in data.iter().enumerate() {
            val |= (*b as u32) << (8 * k);
        }
        let tag = (prefix >> 4) & 0x0f;
        let ty = (prefix >> 2) & 0x03;

        match (ty, tag) {
            // ---- Main items ----
            (0, 0b1000) | (0, 0b1001) | (0, 0b1011) => {
                let kind = match tag {
                    0b1000 => "Input",
                    0b1001 => "Output",
                    _ => "Feature",
                };
                let (s, c) = match (size, count) {
                    (Some(s), Some(c)) => (s, c),
                    _ => {
                        return Err(format!(
                            "offset {}: {} with no Report Size and/or Report Count in scope",
                            i, kind
                        ))
                    }
                };
                let is_constant = val & 0x01 != 0;
                let is_variable = val & 0x02 != 0;
                if !pending.is_empty() && is_variable && !is_constant && pending.len() != c {
                    return Err(format!(
                        "offset {}: {} has Report Count {} but {} Usage items are in scope; a \
                         host maps only the first {} and silently drops the rest",
                        i,
                        kind,
                        c,
                        pending.len(),
                        c
                    ));
                }
                if is_constant && !pending.is_empty() {
                    return Err(format!(
                        "offset {}: {} is Constant (padding) yet {} Usage items are in scope; \
                         padding names no usage",
                        i,
                        kind,
                        pending.len()
                    ));
                }
                if tag == 0b1000 {
                    out.input_bits += s * c;
                    out.input_usages.push(pending.clone());
                }
                pending.clear();
                usage_min = None;
            }
            (0, 0b1010) => {
                depth += 1;
                pending.clear();
                usage_min = None;
            }
            (0, 0b1100) => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!(
                        "offset {}: End Collection with no open Collection",
                        i
                    ));
                }
                pending.clear();
                usage_min = None;
            }
            (0, _) => {
                return Err(format!(
                    "offset {}: 0x{:02x} is a Main item with reserved tag {:#06b}; a host's \
                     parser stops here. This is what writing a two-byte Usage with the \
                     one-byte 0x09 form produces: the extra byte is read as a new item.",
                    i, prefix, tag
                ))
            }

            // ---- Global items ----
            (1, 0) => usage_page = val,
            (1, 1) | (1, 2) | (1, 3) | (1, 4) | (1, 5) | (1, 6) => {}
            (1, 7) => size = Some(val as usize),
            (1, 8) => {}
            (1, 9) => count = Some(val as usize),
            (1, 10) | (1, 11) => {}
            (1, _) => {
                return Err(format!(
                    "offset {}: 0x{:02x} is a Global item with reserved tag {}",
                    i, prefix, tag
                ))
            }

            // ---- Local items ----
            (2, 0) => {
                // A one- or two-byte Usage is page-relative; a four-byte one carries its page.
                let full = if n >= 4 {
                    val
                } else {
                    (usage_page << 16) | val
                };
                pending.push(full);
            }
            (2, 1) => usage_min = Some(val),
            (2, 2) => {
                let lo = usage_min.take().ok_or_else(|| {
                    format!(
                        "offset {}: Usage Maximum with no preceding Usage Minimum",
                        i
                    )
                })?;
                if val < lo {
                    return Err(format!(
                        "offset {}: Usage Maximum 0x{:04x} is below Usage Minimum 0x{:04x}",
                        i, val, lo
                    ));
                }
                for u in lo..=val {
                    pending.push((usage_page << 16) | u);
                }
            }
            (2, 3..=5) | (2, 7..=10) => {}
            (2, _) => {
                return Err(format!(
                    "offset {}: 0x{:02x} is a Local item with reserved tag {}",
                    i, prefix, tag
                ))
            }

            // ---- Reserved item type ----
            _ => {
                return Err(format!(
                    "offset {}: 0x{:02x} has item type 3, which the specification reserves",
                    i, prefix
                ))
            }
        }

        i += 1 + n;
    }

    if depth != 0 {
        return Err(format!("{} Collection item(s) were never closed", depth));
    }
    Ok(out)
}

/// Expand a 16-bit Bluetooth SIG alias to the full 128-bit form the base stack requires.
///
/// The base parses every UUID with `uuid::Uuid::parse_str`, which rejects the shorthand, so
/// the startup examples must spell them out. This is how the tests address them.
pub fn sig_uuid(alias: &str) -> String {
    format!("0000{}-0000-1000-8000-00805f9b34fb", alias.to_lowercase())
}

/// Pull a characteristic's `initial_value` out of the `static_mode` startup example.
///
/// Panics with a readable message rather than returning an `Option`: every caller is
/// asserting the example is well formed, so a missing characteristic is a test failure and
/// not a case to handle.
pub fn example_characteristic_value(static_mode: &serde_json::Value, alias: &str) -> String {
    let want = sig_uuid(alias);
    let handlers = static_mode["event_handlers"]
        .as_array()
        .expect("static_mode has no event_handlers array");
    for handler in handlers {
        let actions = match handler["handler"]["actions"].as_array() {
            Some(a) => a,
            None => continue,
        };
        for action in actions {
            if action["type"] != "add_service" {
                continue;
            }
            let chars = match action["characteristics"].as_array() {
                Some(c) => c,
                None => continue,
            };
            for ch in chars {
                if ch["uuid"].as_str() == Some(want.as_str()) {
                    return ch["initial_value"]
                        .as_str()
                        .unwrap_or_else(|| {
                            panic!("characteristic {want} has no string initial_value")
                        })
                        .to_string();
                }
            }
        }
    }
    panic!("no characteristic {want} in the static_mode startup example");
}

/// Assert the HID Information characteristic (0x2A4A) really says HID v1.11.
///
/// Every GATT integer is little-endian, so bcdHID 0x0111 is the bytes `11 01`. Written the
/// other way round — which all four HID profiles did — a host reads HID version 17.01.
pub fn assert_hid_information(value: &str, what: &str) {
    let bytes = hex::decode(value)
        .unwrap_or_else(|e| panic!("{what}: HID Information is not valid hex: {e}"));
    assert_eq!(
        bytes.len(),
        4,
        "{what}: HID Information is 4 octets (bcdHID uint16, bCountryCode uint8, Flags uint8), \
         got {}",
        bytes.len()
    );
    let bcd = u16::from_le_bytes([bytes[0], bytes[1]]);
    assert_eq!(
        bcd,
        0x0111,
        "{what}: bcdHID decodes to 0x{bcd:04x} (version {}.{:02x}), not 0x0111 (v1.11). GATT \
         integers are little-endian, so the bytes must be `1101`.",
        bcd >> 8,
        bcd & 0xff
    );
    assert_eq!(
        bytes[2], 0x00,
        "{what}: bCountryCode should be 0x00 (not localised)"
    );
    assert_eq!(
        bytes[3], 0x02,
        "{what}: Flags should be 0x02 (NormallyConnectable)"
    );
}

/// Walk a descriptor and assert it is well formed and describes exactly `expect_len` bytes.
pub fn assert_describes_report_of(desc: &[u8], expect_len: usize, what: &str) {
    let walked = match walk(desc) {
        Ok(w) => w,
        Err(e) => panic!("{what}: report descriptor is malformed: {e}"),
    };
    let got = walked
        .input_report_len()
        .unwrap_or_else(|e| panic!("{what}: {e}"));
    assert_eq!(
        got, expect_len,
        "{what}: the descriptor describes a {got}-byte input report, but the profile publishes \
         a {expect_len}-byte one. A host reads the descriptor, so the descriptor is right and \
         the published length is wrong (or vice versa) — they cannot both ship."
    );
}
