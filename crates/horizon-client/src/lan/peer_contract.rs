//! Dormant bounded compatibility claims. This wire format is separate from the
//! internal snapshot hashing format; no network or admission caller uses it.
use super::data::RegistrySnapshot;
use std::{error::Error, fmt};

pub const MAX_WIRE_BYTES: usize = 8_192;
const MAGIC: &[u8; 8] = b"HZPCONTR";
const FIXED_BYTES: usize = 254;
const MAX_TEXT: usize = 1_024;
const MAX_OWNER: usize = 64;
// These codes are explicit v1 definitions, not extensible feature flags:
// wire 1, one paired-preview-1/current-schema profile, the
// horizon-peer-snapshot-1 component algorithms in data.rs, and
// paired-folder-sha256-1 package identity, respectively.
const CODES: [u16; 4] = [1, 1, 1, 1];

/// Unauthenticated DATA only. A caller may make false claims. Construction,
/// decoding and equality establish neither local capture provenance nor peer,
/// executable, world or writable-state authority. This type cannot create the
/// game binary's frozen local-input holder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireContract {
    protocol: i32,
    game: String,
    build: [u8; 32],
    package_id: String,
    package: [u8; 32],
    machine_id: String,
    schema: u32,
    registries: RegistrySnapshot,
}

/// Borrowed claims, checked before any owned strings are allocated. Digests do
/// not attest to the existence or execution of the named inputs.
pub struct WireClaims<'a> {
    pub protocol: i32,
    pub game: &'a str,
    pub build: [u8; 32],
    pub package_id: &'a str,
    pub package: [u8; 32],
    pub machine_id: &'a str,
    pub schema: u32,
    pub registries: &'a RegistrySnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractError {
    Limit,
    Truncated,
    Magic,
    Unsupported(&'static str),
    Invalid(&'static str),
    Trailing,
    Overflow,
}
impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit => f.write_str("wire contract byte limit"),
            Self::Truncated => f.write_str("truncated wire contract"),
            Self::Magic => f.write_str("wire contract magic"),
            Self::Unsupported(field) => write!(f, "unsupported wire contract {field}"),
            Self::Invalid(field) => write!(f, "invalid wire contract {field}"),
            Self::Trailing => f.write_str("trailing wire contract bytes"),
            Self::Overflow => f.write_str("wire contract length overflow"),
        }
    }
}
impl Error for ContractError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractMismatch {
    ProtocolGame,
    Executable,
    PackageContent,
    MachineSchema,
    Builtin,
    Blocks,
    Synchronized,
    Tags,
    Features,
}

fn checked_length(claims: &WireClaims<'_>) -> Result<usize, ContractError> {
    if claims.game.is_empty() || claims.game.len() > MAX_TEXT {
        return Err(ContractError::Invalid("game version"));
    }
    if claims.package_id.is_empty()
        || claims.package_id.len() > MAX_OWNER
        || !claims
            .package_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(ContractError::Invalid("package ID"));
    }
    if claims.machine_id.is_empty() || claims.machine_id.len() > MAX_TEXT {
        return Err(ContractError::Invalid("machine ID"));
    }
    let valid_machine = claims
        .machine_id
        .split_once(':')
        .is_some_and(|(owner, path)| {
            owner == claims.package_id
                && !path.is_empty()
                && path
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_./-".contains(&b))
        });
    if !valid_machine {
        return Err(ContractError::Invalid("machine ID"));
    }
    if claims.schema != 1 {
        return Err(ContractError::Unsupported("machine schema"));
    }
    let length = [
        claims.game.len(),
        claims.package_id.len(),
        claims.machine_id.len(),
    ]
    .into_iter()
    .try_fold(FIXED_BYTES, |n, len| n.checked_add(len))
    .ok_or(ContractError::Overflow)?;
    if length > MAX_WIRE_BYTES {
        return Err(ContractError::Limit);
    }
    Ok(length)
}

impl WireContract {
    pub fn from_claims(claims: WireClaims<'_>) -> Result<Self, ContractError> {
        checked_length(&claims)?;
        Ok(Self {
            protocol: claims.protocol,
            game: claims.game.into(),
            build: claims.build,
            package_id: claims.package_id.into(),
            package: claims.package,
            machine_id: claims.machine_id.into(),
            schema: claims.schema,
            registries: claims.registries.clone(),
        })
    }

    pub fn claims(&self) -> WireClaims<'_> {
        WireClaims {
            protocol: self.protocol,
            game: &self.game,
            build: self.build,
            package_id: &self.package_id,
            package: self.package,
            machine_id: &self.machine_id,
            schema: self.schema,
            registries: &self.registries,
        }
    }

    /// The ceiling is for this codec only, not earlier network frame allocation
    /// or I/O time. No owned strings are made until the entire input is checked.
    pub fn decode(bytes: &[u8]) -> Result<Self, ContractError> {
        if bytes.len() > MAX_WIRE_BYTES {
            return Err(ContractError::Limit);
        }
        let mut reader = Cursor { bytes, offset: 0 };
        if reader.take(8)? != MAGIC {
            return Err(ContractError::Magic);
        }
        for (expected, field) in CODES.into_iter().zip([
            "version",
            "profile",
            "snapshot semantics",
            "package identity scheme",
        ]) {
            if reader.u16()? != expected {
                return Err(ContractError::Unsupported(field));
            }
        }
        let protocol = i32::from_be_bytes(reader.array()?);
        let game = reader.text(MAX_TEXT, "game version")?;
        let build = reader.array()?;
        let package_id = reader.text(MAX_OWNER, "package ID")?;
        let package = reader.array()?;
        let machine_id = reader.text(MAX_TEXT, "machine ID")?;
        let schema = u32::from_be_bytes(reader.array()?);
        let registries = RegistrySnapshot {
            builtin: reader.array()?,
            blocks: reader.array()?,
            synchronized: reader.array()?,
            tags: reader.array()?,
            features: reader.array()?,
        };
        if reader.offset != bytes.len() {
            return Err(ContractError::Trailing);
        }
        Self::from_claims(WireClaims {
            protocol,
            game,
            build,
            package_id,
            package,
            machine_id,
            schema,
            registries: &registries,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, ContractError> {
        let length = checked_length(&self.claims())?;
        let mut out = Vec::with_capacity(length);
        out.extend_from_slice(MAGIC);
        for code in CODES {
            out.extend_from_slice(&code.to_be_bytes());
        }
        out.extend_from_slice(&self.protocol.to_be_bytes());
        for (text, digest) in [(&self.game, &self.build), (&self.package_id, &self.package)] {
            put_text(&mut out, text)?;
            out.extend_from_slice(digest);
        }
        put_text(&mut out, &self.machine_id)?;
        out.extend_from_slice(&self.schema.to_be_bytes());
        for digest in [
            &self.registries.builtin,
            &self.registries.blocks,
            &self.registries.synchronized,
            &self.registries.tags,
            &self.registries.features,
        ] {
            out.extend_from_slice(digest);
        }
        debug_assert_eq!(out.len(), length);
        Ok(out)
    }

    /// Component equality only; the result is not a validation receipt.
    pub fn compare(&self, other: &Self) -> Result<(), ContractMismatch> {
        use ContractMismatch::*;
        if self.protocol != other.protocol || self.game != other.game {
            return Err(ProtocolGame);
        }
        if self.build != other.build {
            return Err(Executable);
        }
        if self.package_id != other.package_id || self.package != other.package {
            return Err(PackageContent);
        }
        if self.machine_id != other.machine_id || self.schema != other.schema {
            return Err(MachineSchema);
        }
        for (a, b, mismatch) in [
            (&self.registries.builtin, &other.registries.builtin, Builtin),
            (&self.registries.blocks, &other.registries.blocks, Blocks),
            (
                &self.registries.synchronized,
                &other.registries.synchronized,
                Synchronized,
            ),
            (&self.registries.tags, &other.registries.tags, Tags),
            (
                &self.registries.features,
                &other.registries.features,
                Features,
            ),
        ] {
            if a != b {
                return Err(mismatch);
            }
        }
        Ok(())
    }
}

fn put_text(out: &mut Vec<u8>, text: &str) -> Result<(), ContractError> {
    let len = u16::try_from(text.len()).map_err(|_| ContractError::Overflow)?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], ContractError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(ContractError::Overflow)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(ContractError::Truncated)?;
        self.offset = end;
        Ok(value)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], ContractError> {
        self.take(N)?
            .try_into()
            .map_err(|_| ContractError::Truncated)
    }
    fn u16(&mut self) -> Result<u16, ContractError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn text(&mut self, maximum: usize, field: &'static str) -> Result<&'a str, ContractError> {
        let length = usize::from(self.u16()?);
        if length == 0 || length > maximum {
            return Err(ContractError::Invalid(field));
        }
        std::str::from_utf8(self.take(length)?).map_err(|_| ContractError::Invalid(field))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independently authored v1 fixture: protocol 0x01020304, g / p / p:c,
    // build 00..1f, package 20..3f, then five consecutive 32-byte ranges.
    const GOLDEN: &[u8] = &[
        0x48, 0x5a, 0x50, 0x43, 0x4f, 0x4e, 0x54, 0x52, 0, 1, 0, 1, 0, 1, 0, 1, 1, 2, 3, 4, 0, 1,
        b'g', 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
        0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
        0x1d, 0x1e, 0x1f, 0, 1, b'p', 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29,
        0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38,
        0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0, 3, b'p', b':', b'c', 0, 0, 0, 1, 0x40, 0x41,
        0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50,
        0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f,
        0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e,
        0x6f, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d,
        0x7e, 0x7f, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c,
        0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b,
        0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa,
        0xab, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9,
        0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8,
        0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf, 0xd0, 0xd1, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7,
        0xd8, 0xd9, 0xda, 0xdb, 0xdc, 0xdd, 0xde, 0xdf,
    ];

    #[test]
    fn literal_v1_fixture_fields_offsets_and_canonical_encoding() {
        assert_eq!(GOLDEN.len(), 259);
        assert_eq!(&GOLDEN[16..23], &[1, 2, 3, 4, 0, 1, b'g']);
        assert_eq!(&GOLDEN[55..58], &[0, 1, b'p']);
        assert_eq!(&GOLDEN[90..99], &[0, 3, b'p', b':', b'c', 0, 0, 0, 1]);
        let value = WireContract::decode(GOLDEN).unwrap();
        let fields = value.claims();
        assert_eq!(fields.protocol, 0x01020304);
        assert_eq!(
            (
                fields.game,
                fields.package_id,
                fields.machine_id,
                fields.schema
            ),
            ("g", "p", "p:c", 1)
        );
        for (digest, start, offset) in [
            (&fields.build, 0u8, 23),
            (&fields.package, 32, 58),
            (&fields.registries.builtin, 64, 99),
            (&fields.registries.blocks, 96, 131),
            (&fields.registries.synchronized, 128, 163),
            (&fields.registries.tags, 160, 195),
            (&fields.registries.features, 192, 227),
        ] {
            let expected: [u8; 32] = std::array::from_fn(|i| start + i as u8);
            assert_eq!(digest, &expected);
            assert_eq!(&GOLDEN[offset..offset + 32], &expected);
        }
        assert_eq!(value.encode().unwrap(), GOLDEN);
        assert_eq!(value.encode().unwrap(), value.encode().unwrap());
        assert_eq!(
            WireContract::decode(&value.encode().unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn every_truncation_and_unknown_code_or_trailing_data_refuses() {
        for end in 0..GOLDEN.len() {
            assert!(
                WireContract::decode(&GOLDEN[..end]).is_err(),
                "prefix {end}"
            );
        }
        let mut bytes = GOLDEN.to_vec();
        bytes[0] ^= 1;
        assert_eq!(WireContract::decode(&bytes), Err(ContractError::Magic));
        for offset in [8, 10, 12, 14] {
            for code in [0u16, 2, u16::MAX] {
                let mut bytes = GOLDEN.to_vec();
                bytes[offset..offset + 2].copy_from_slice(&code.to_be_bytes());
                assert!(matches!(
                    WireContract::decode(&bytes),
                    Err(ContractError::Unsupported(_))
                ));
            }
        }
        for schema in [0u32, 2, u32::MAX] {
            let mut bytes = GOLDEN.to_vec();
            bytes[95..99].copy_from_slice(&schema.to_be_bytes());
            assert_eq!(
                WireContract::decode(&bytes),
                Err(ContractError::Unsupported("machine schema"))
            );
        }
        let mut bytes = GOLDEN.to_vec();
        bytes.push(0);
        assert_eq!(WireContract::decode(&bytes), Err(ContractError::Trailing));
        bytes.resize(MAX_WIRE_BYTES, 0);
        assert_eq!(WireContract::decode(&bytes), Err(ContractError::Trailing));
        bytes.push(0);
        assert_eq!(WireContract::decode(&bytes), Err(ContractError::Limit));
    }

    #[test]
    fn borrowed_claims_and_decoder_enforce_lengths_utf8_and_owner_grammar() {
        for (offset, excessive) in [(20, 1025u16), (55, 65), (90, 1025)] {
            for length in [0, excessive, u16::MAX] {
                let mut bytes = GOLDEN.to_vec();
                bytes[offset..offset + 2].copy_from_slice(&length.to_be_bytes());
                assert!(matches!(
                    WireContract::decode(&bytes),
                    Err(ContractError::Invalid(_))
                ));
            }
        }
        for offset in [22, 57, 92] {
            let mut bytes = GOLDEN.to_vec();
            bytes[offset] = 0xff;
            assert!(matches!(
                WireContract::decode(&bytes),
                Err(ContractError::Invalid(_))
            ));
        }
        let value = WireContract::decode(GOLDEN).unwrap();
        for owner in ["", "P", "p-q", "p.q", "p/q", "p:q", "p q", "é"] {
            let mut fields = value.claims();
            fields.package_id = owner;
            assert_eq!(
                WireContract::from_claims(fields),
                Err(ContractError::Invalid("package ID"))
            );
        }
        for machine in ["", "p", ":c", "p:", "q:c", "p:c:d", "p:C", "p:c\\d", "p:é"] {
            let mut fields = value.claims();
            fields.machine_id = machine;
            assert_eq!(
                WireContract::from_claims(fields),
                Err(ContractError::Invalid("machine ID"))
            );
        }
        for game in ["", &"x".repeat(1025), &"雪".repeat(342)] {
            let mut fields = value.claims();
            fields.game = game;
            assert_eq!(
                WireContract::from_claims(fields),
                Err(ContractError::Invalid("game version"))
            );
        }
        for offset in [57, 94] {
            let mut bytes = GOLDEN.to_vec();
            bytes[offset] = b'-';
            if offset == 94 {
                bytes[92] = b'q';
            }
            assert!(matches!(
                WireContract::decode(&bytes),
                Err(ContractError::Invalid(_))
            ));
        }
    }

    #[test]
    fn maximum_grammar_bound_and_utf8_byte_lengths_are_exact() {
        let value = WireContract::decode(GOLDEN).unwrap();
        let owner = "p".repeat(64);
        let machine = format!("{owner}:{}", "x".repeat(959));
        let game = "x".repeat(1024);
        let mut fields = value.claims();
        fields.game = &game;
        fields.package_id = &owner;
        fields.machine_id = &machine;
        let maximum = WireContract::from_claims(fields).unwrap();
        assert_eq!(maximum.encode().unwrap().len(), 2366);
        assert_eq!(
            WireContract::decode(&maximum.encode().unwrap()).unwrap(),
            maximum
        );
        let mut fields = value.claims();
        fields.game = "雪";
        let unicode = WireContract::from_claims(fields).unwrap();
        assert_eq!(
            &unicode.encode().unwrap()[20..25],
            &[0, 3, 0xe9, 0x9b, 0xaa]
        );
        assert_eq!(
            WireContract::decode(&unicode.encode().unwrap()).unwrap(),
            unicode
        );
        let mut fields = value.claims();
        fields.game = horizon_protocol::GAME_VERSION;
        assert!(WireContract::from_claims(fields).is_ok());
        // Even an internally altered value must pass the encoder preflight.
        let mut invalid = value.clone();
        invalid.package_id = "p".repeat(65);
        assert_eq!(invalid.encode(), Err(ContractError::Invalid("package ID")));
        let mut invalid = value;
        invalid.machine_id = "p:".to_owned() + &"x".repeat(1023);
        assert_eq!(invalid.encode(), Err(ContractError::Invalid("machine ID")));
    }

    #[test]
    fn comparison_identifies_every_component_without_a_receipt() {
        use ContractMismatch::*;
        let value = WireContract::decode(GOLDEN).unwrap();
        let mut controls = Vec::new();
        let mut changed = value.clone();
        changed.protocol = i32::MIN;
        controls.push((changed, ProtocolGame));
        let mut changed = value.clone();
        changed.game = "other".into();
        controls.push((changed, ProtocolGame));
        let mut changed = value.clone();
        changed.build[0] ^= 1;
        controls.push((changed, Executable));
        let mut changed = value.clone();
        changed.package[0] ^= 1;
        controls.push((changed, PackageContent));
        let mut changed = value.clone();
        changed.package_id = "q".into();
        changed.machine_id = "q:c".into();
        controls.push((changed, PackageContent));
        let mut changed = value.clone();
        changed.machine_id = "p:d".into();
        controls.push((changed, MachineSchema));
        for (index, mismatch) in [Builtin, Blocks, Synchronized, Tags, Features]
            .into_iter()
            .enumerate()
        {
            let mut changed = value.clone();
            let digests = [
                &mut changed.registries.builtin,
                &mut changed.registries.blocks,
                &mut changed.registries.synchronized,
                &mut changed.registries.tags,
                &mut changed.registries.features,
            ];
            digests[index][0] ^= 1;
            controls.push((changed, mismatch));
        }
        for (changed, mismatch) in controls {
            let decoded = WireContract::decode(&changed.encode().unwrap()).unwrap();
            assert_eq!(value.compare(&decoded), Err(mismatch));
            assert_eq!(decoded.compare(&value), Err(mismatch));
        }
        assert_eq!(
            value.compare(&WireContract::decode(GOLDEN).unwrap()),
            Ok(())
        );
    }
}
