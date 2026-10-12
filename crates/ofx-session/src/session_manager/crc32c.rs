const POLYNOMIAL: u32 = 0x82F6_3B78;
const TABLES: [[u32; 256]; 8] = tables();

const fn tables() -> [[u32; 256]; 8] {
    let mut tables = [[0_u32; 256]; 8];
    let mut byte: u32 = 0;
    while byte < 256 {
        let mut crc = byte;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ POLYNOMIAL
            };
            bit += 1;
        }
        tables[0][byte as usize] = crc;
        byte += 1;
    }
    let mut table = 1;
    while table < 8 {
        let mut index = 0;
        while index < 256 {
            let previous = tables[table - 1][index];
            tables[table][index] = (previous >> 8) ^ tables[0][(previous & 0xff) as usize];
            index += 1;
        }
        table += 1;
    }
    tables
}

pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        let low = u32::from_le_bytes([word[0], word[1], word[2], word[3]]) ^ crc;
        let high = u32::from_le_bytes([word[4], word[5], word[6], word[7]]);
        crc = TABLES[7][(low & 0xff) as usize]
            ^ TABLES[6][((low >> 8) & 0xff) as usize]
            ^ TABLES[5][((low >> 16) & 0xff) as usize]
            ^ TABLES[4][(low >> 24) as usize]
            ^ TABLES[3][(high & 0xff) as usize]
            ^ TABLES[2][((high >> 8) & 0xff) as usize]
            ^ TABLES[1][((high >> 16) & 0xff) as usize]
            ^ TABLES[0][(high >> 24) as usize];
    }
    for &byte in words.remainder() {
        crc = TABLES[0][((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytewise(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 0 {
                    crc >> 1
                } else {
                    (crc >> 1) ^ POLYNOMIAL
                };
            }
        }
        !crc
    }

    #[test]
    fn crc32c_matches_the_standard_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn crc32c_equals_the_byte_at_a_time_crc32c_at_every_length_and_alignment() {
        let bytes: Vec<u8> = (0..300_u32)
            .map(|index| u8::try_from((index * 7 + 13) % 251).unwrap())
            .collect();
        for start in 0..9 {
            for end in start..bytes.len() {
                assert_eq!(crc32c(&bytes[start..end]), bytewise(&bytes[start..end]));
            }
        }
    }
}
