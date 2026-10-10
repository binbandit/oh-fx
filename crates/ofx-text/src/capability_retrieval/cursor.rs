use super::{Document, Domain, Request};

const SECRET: [u64; 4] = [
    0xa076_1d64_78bd_642f,
    0xe703_7ed1_a0b4_28db,
    0x8ebc_6af0_9c88_c6e3,
    0x5899_65cc_7537_4cc3,
];

pub(super) fn render(domain: Domain, fingerprint: u64, request_hash: u64, offset: usize) -> String {
    let domain = match domain {
        Domain::Skill => 's',
        Domain::Mcp => 'm',
    };
    format!("c1:{domain}:{fingerprint:x}:{request_hash:x}:{offset}")
}

pub(super) fn fingerprint(documents: &[Document<'_>]) -> u64 {
    let mut xor = 0;
    let mut sum = 0u64;
    for document in documents {
        let mut fields = Vec::new();
        append(&mut fields, document.stable_key);
        for field in document
            .identities
            .iter()
            .copied()
            .chain(document.primary_fields())
            .chain(document.secondary.iter().copied())
        {
            append(&mut fields, field);
        }
        let digest = hash(0x6361_7061_6269_6c69, &fields);
        xor ^= digest;
        sum = sum.wrapping_add(digest.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    }
    xor ^ sum.rotate_left(17) ^ u64::try_from(documents.len()).unwrap_or(u64::MAX)
}

pub(super) fn request_hash(request: Request<'_>, domain: Domain) -> u64 {
    let mut fields = Vec::new();
    append(&mut fields, request.query.raw());
    fields.push(match domain {
        Domain::Skill => 0,
        Domain::Mcp => 1,
    });
    fields.push(1);
    if let Some(server) = request.server {
        append(&mut fields, server);
    }
    hash(0x7265_7175_6573_7421, &fields)
}

fn append(output: &mut Vec<u8>, value: &str) {
    output.extend(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    output.extend(value.as_bytes());
}

fn read(bytes: &[u8]) -> u64 {
    let mut buffer = [0; 8];
    buffer[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(buffer)
}

fn product(left: u64, right: u64) -> (u64, u64) {
    let bytes = (u128::from(left) * u128::from(right)).to_le_bytes();
    (read(&bytes[..8]), read(&bytes[8..]))
}

fn mix(left: u64, right: u64) -> u64 {
    let (a, b) = product(left, right);
    a ^ b
}

fn hash(seed: u64, input: &[u8]) -> u64 {
    let initial = seed ^ mix(seed ^ SECRET[0], SECRET[1]);
    let mut state = [initial; 3];
    let (mut a, mut b) = if input.len() <= 16 {
        if input.len() >= 4 {
            let end = input.len() - 4;
            let quarter = (input.len() >> 3) << 2;
            (
                (read(&input[..4]) << 32) | read(&input[quarter..quarter + 4]),
                (read(&input[end..]) << 32) | read(&input[end - quarter..end - quarter + 4]),
            )
        } else if input.is_empty() {
            (0, 0)
        } else {
            (
                (u64::from(input[0]) << 16)
                    | (u64::from(input[input.len() >> 1]) << 8)
                    | u64::from(input[input.len() - 1]),
                0,
            )
        }
    } else {
        let mut offset = 0;
        while offset + 48 < input.len() {
            for i in 0..3 {
                state[i] = mix(
                    read(&input[offset + 16 * i..offset + 16 * i + 8]) ^ SECRET[i + 1],
                    read(&input[offset + 16 * i + 8..offset + 16 * i + 16]) ^ state[i],
                );
            }
            offset += 48;
        }
        if input.len() >= 48 {
            state[0] ^= state[1] ^ state[2];
        }
        while offset + 16 < input.len() {
            state[0] = mix(
                read(&input[offset..offset + 8]) ^ SECRET[1],
                read(&input[offset + 8..offset + 16]) ^ state[0],
            );
            offset += 16;
        }
        (
            read(&input[input.len() - 16..input.len() - 8]),
            read(&input[input.len() - 8..]),
        )
    };
    a ^= SECRET[1];
    b ^= state[0];
    let (a, b) = product(a, b);
    mix(
        a ^ SECRET[0] ^ u64::try_from(input.len()).unwrap_or(u64::MAX),
        b ^ SECRET[1],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PreparedQuery;

    #[test]
    fn private_cursor_matches_pinned_retrieval_for_seven_skills() {
        let names: Vec<_> = (0..7).map(|i| format!("skill-{i}")).collect();
        let paths: Vec<_> = (0..7).map(|i| format!("/{i}")).collect();
        let documents: Vec<_> = names
            .iter()
            .zip(&paths)
            .map(|(name, path)| Document {
                identities: [name, ""],
                stable_key: path,
                primary: [name, "", "", ""],
                secondary: ["description", "", ""],
                ..Document::default()
            })
            .collect();
        let query = PreparedQuery::prepare(String::new()).unwrap();
        let request = Request {
            query: &query,
            server: None,
        };
        assert_eq!(
            render(
                Domain::Skill,
                fingerprint(&documents),
                request_hash(request, Domain::Skill),
                5
            ),
            "c1:s:2e2e1cae8e5ca8b4:2b7dc6aa7abcd396:5"
        );
    }

    #[test]
    fn private_hash_matches_bundled_source_vectors() {
        for (seed, input, expected) in [
            (0, "", 0x0409_638e_e2bd_e459),
            (1, "a", 0xa841_2d09_1b5f_e0a9),
            (2, "abc", 0x32dd_92e4_b291_5153),
            (3, "message digest", 0x8619_1240_89a3_a16b),
            (4, "abcdefghijklmnopqrstuvwxyz", 0x7a43_afb6_1d7f_5f40),
            (
                5,
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                0xff42_329b_90e5_0d58,
            ),
            (
                6,
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                0xc39c_ab13_b115_aad3,
            ),
        ] {
            assert_eq!(hash(seed, input.as_bytes()), expected);
        }
    }
}
