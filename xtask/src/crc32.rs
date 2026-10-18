//! CRC-32 (IEEE 802.3, reflected, polynomial 0xEDB88320), as GPT requires.

const TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        #[allow(clippy::cast_possible_truncation)]
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

pub(crate) fn crc32(data: &[u8]) -> u32 {
    !data
        .iter()
        .fold(!0u32, |c, &b| TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8))
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn known_answers() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414f_a339);
    }
}
