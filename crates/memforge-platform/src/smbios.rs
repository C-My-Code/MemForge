//! Pure SMBIOS parser (type 17, Memory Device). Never panics on malformed input.

use crate::DimmInfo;

fn u16_at(d: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        d.get(off..off.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32_at(d: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        d.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// SMBIOS memory device type byte to a display name.
pub fn memory_type_name(t: u8) -> String {
    match t {
        0x01 => "Other",
        0x02 => "Unknown",
        0x03 => "DRAM",
        0x04 => "EDRAM",
        0x05 => "VRAM",
        0x06 => "SRAM",
        0x07 => "RAM",
        0x08 => "ROM",
        0x09 => "FLASH",
        0x0A => "EEPROM",
        0x0B => "FEPROM",
        0x0C => "EPROM",
        0x0D => "CDRAM",
        0x0E => "3DRAM",
        0x0F => "SDRAM",
        0x10 => "SGRAM",
        0x11 => "RDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x19 => "FBD2",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x1F => "Logical non-volatile device",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => return format!("Unknown(0x{t:02X})"),
    }
    .to_string()
}

/// Parse a `RawSMBIOSData` blob as returned by `GetSystemFirmwareTable('RSMB')`:
/// an 8-byte header (method, major, minor, revision, u32 length) then structures.
pub fn parse_raw_smbios(raw: &[u8]) -> Vec<DimmInfo> {
    if raw.len() < 8 {
        return Vec::new();
    }
    let declared = u32_at(raw, 4).unwrap_or(0) as usize;
    let body = &raw[8..];
    let body = &body[..declared.min(body.len())];
    parse_structures(body)
}

/// Parse the SMBIOS structure table (no RawSMBIOSData header).
pub fn parse_structures(table: &[u8]) -> Vec<DimmInfo> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= table.len() {
        let typ = table[pos];
        let len = table[pos + 1] as usize;
        if len < 4 || pos + len > table.len() {
            break;
        }
        let formatted = &table[pos..pos + len];
        // String area: NUL-terminated strings ended by an extra NUL (an empty
        // area is just two NULs).
        let sstart = pos + len;
        let mut strings: Vec<&[u8]> = Vec::new();
        let mut p = sstart;
        loop {
            if p >= table.len() {
                p = table.len();
                break;
            }
            if table[p] == 0 {
                p += if strings.is_empty() { 2 } else { 1 };
                break;
            }
            let end = table[p..]
                .iter()
                .position(|&c| c == 0)
                .map_or(table.len(), |e| p + e);
            strings.push(&table[p..end]);
            p = end + 1;
        }
        if typ == 17 {
            if let Some(d) = parse_type17(formatted, &strings) {
                out.push(d);
            }
        }
        if typ == 127 {
            break;
        }
        pos = p.max(pos + len);
    }
    out
}

fn parse_type17(f: &[u8], strings: &[&[u8]]) -> Option<DimmInfo> {
    let s = |idx_off: usize| -> String {
        match f.get(idx_off) {
            Some(&i) if i >= 1 => strings
                .get(i as usize - 1)
                .map(|b| String::from_utf8_lossy(b).trim().to_string())
                .unwrap_or_default(),
            _ => String::new(),
        }
    };
    let raw_size = u16_at(f, 0x0C)?;
    let size_bytes: u64 = match raw_size {
        0 | 0xFFFF => return None,
        0x7FFF => u64::from(u32_at(f, 0x1C)? & 0x7FFF_FFFF) * 1024 * 1024,
        v if v & 0x8000 != 0 => u64::from(v & 0x7FFF) * 1024,
        v => u64::from(v) * 1024 * 1024,
    };
    if size_bytes == 0 {
        return None;
    }
    let mut speed = u16_at(f, 0x15).map_or(0, u32::from);
    if speed == 0xFFFF {
        speed = u32_at(f, 0x54).map_or(0, |v| v & 0x7FFF_FFFF);
    }
    let mut cfg = u16_at(f, 0x20).map_or(0, u32::from);
    if cfg == 0xFFFF {
        cfg = u32_at(f, 0x58).map_or(0, |v| v & 0x7FFF_FFFF);
    }
    Some(DimmInfo {
        locator: s(0x10),
        bank_locator: s(0x11),
        manufacturer: s(0x17),
        serial_number: s(0x18),
        part_number: s(0x1A),
        size_bytes,
        memory_type: f
            .get(0x12)
            .map(|&t| memory_type_name(t))
            .unwrap_or_default(),
        speed_mts: speed,
        configured_speed_mts: cfg,
        configured_voltage_mv: u16_at(f, 0x26).map_or(0, u32::from),
        rank: f.get(0x1B).map_or(0, |a| a & 0x0F),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type17(size: u16, ext: u32, strings: &[&str], mtype: u8) -> Vec<u8> {
        let mut f = vec![0u8; 0x5C];
        f[0] = 17;
        f[1] = 0x5C;
        f[0x0C..0x0E].copy_from_slice(&size.to_le_bytes());
        f[0x10] = 1; // locator
        f[0x11] = 2; // bank
        f[0x12] = mtype;
        f[0x15..0x17].copy_from_slice(&0xFFFFu16.to_le_bytes());
        f[0x54..0x58].copy_from_slice(&6400u32.to_le_bytes());
        f[0x17] = 3;
        f[0x18] = 4;
        f[0x1A] = 5;
        f[0x1B] = 0x02;
        f[0x20..0x22].copy_from_slice(&6000u16.to_le_bytes());
        f[0x26..0x28].copy_from_slice(&1350u16.to_le_bytes());
        f[0x1C..0x20].copy_from_slice(&ext.to_le_bytes());
        for s in strings {
            f.extend_from_slice(s.as_bytes());
            f.push(0);
        }
        f.push(0);
        f
    }

    fn table() -> Vec<u8> {
        let mut t = Vec::new();
        // type 0 with no strings
        t.extend_from_slice(&[0, 4, 0, 0, 0, 0]);
        t.extend(type17(
            0x7FFF,
            32768,
            &["DIMM A1", "BANK 0", "G.Skill ", "SN123", " F5-6000 "],
            0x22,
        ));
        t.extend(type17(0, 0, &["DIMM A2"], 0x22)); // empty slot
        t.extend(type17(
            16384,
            0,
            &["DIMM B1", "BANK 1", "Acme", "SN9", "PN9"],
            0x1A,
        ));
        t.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
        t
    }

    #[test]
    fn parses_synthetic() {
        let d = parse_structures(&table());
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].locator, "DIMM A1");
        assert_eq!(d[0].manufacturer, "G.Skill");
        assert_eq!(d[0].part_number, "F5-6000");
        assert_eq!(d[0].size_bytes, 32 << 30);
        assert_eq!(d[0].memory_type, "DDR5");
        assert_eq!(d[0].speed_mts, 6400);
        assert_eq!(d[0].configured_speed_mts, 6000);
        assert_eq!(d[0].configured_voltage_mv, 1350);
        assert_eq!(d[0].rank, 2);
        assert_eq!(d[1].memory_type, "DDR4");
        assert_eq!(d[1].size_bytes, 16 << 30);
    }

    #[test]
    fn raw_header_and_malformed() {
        let t = table();
        let mut raw = vec![0, 3, 6, 0];
        raw.extend_from_slice(&(t.len() as u32).to_le_bytes());
        raw.extend_from_slice(&t);
        assert_eq!(parse_raw_smbios(&raw).len(), 2);
        for n in 0..raw.len() {
            let _ = parse_raw_smbios(&raw[..n]);
        }
        let junk: Vec<u8> = (0..500u32).map(|i| (i * 31 % 251) as u8).collect();
        let _ = parse_structures(&junk);
        let _ = parse_raw_smbios(&[0xFF; 64]);
    }
}
