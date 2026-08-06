use crate::model::MAX_TLS_RECORD_BODY;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsParseResult {
    Sni(String),
    NeedMoreData { required_total: usize },
    NotClientHello,
    ClientHelloWithoutSni,
    Invalid(TlsParseError),
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum TlsParseError {
    #[error("unsupported TLS record version {major:#04x}.{minor:#04x}")]
    UnsupportedRecordVersion { major: u8, minor: u8 },

    #[error("TLS record body length {0} exceeds configured maximum")]
    RecordTooLarge(usize),

    #[error("TLS handshake length {0} exceeds configured maximum")]
    HandshakeTooLarge(usize),

    #[error("ClientHello spans multiple TLS records")]
    HandshakeSpansMultipleRecords,

    #[error("invalid ClientHello field: {0}")]
    InvalidField(&'static str),

    #[error("too many TLS extensions")]
    TooManyExtensions,

    #[error("invalid SNI extension")]
    InvalidSniExtension,

    #[error("invalid SNI hostname")]
    InvalidHostname,
}

fn read_u8(
    input: &[u8],
    offset: &mut usize,
    end: usize,
    field: &'static str,
) -> Result<u8, TlsParseError> {
    if *offset >= end {
        return Err(TlsParseError::InvalidField(field));
    }
    let val = input[*offset];
    *offset += 1;
    Ok(val)
}

fn read_be_u16(
    input: &[u8],
    offset: &mut usize,
    end: usize,
    field: &'static str,
) -> Result<u16, TlsParseError> {
    if *offset + 2 > end {
        return Err(TlsParseError::InvalidField(field));
    }
    let val = u16::from_be_bytes([input[*offset], input[*offset + 1]]);
    *offset += 2;
    Ok(val)
}

fn take<'a>(
    input: &'a [u8],
    offset: &mut usize,
    len: usize,
    end: usize,
    field: &'static str,
) -> Result<&'a [u8], TlsParseError> {
    if *offset + len > end {
        return Err(TlsParseError::InvalidField(field));
    }
    let slice = &input[*offset..*offset + len];
    *offset += len;
    Ok(slice)
}

pub fn parse_client_hello_sni(input: &[u8]) -> TlsParseResult {
    if input.len() < 5 {
        return TlsParseResult::NeedMoreData { required_total: 5 };
    }
    if input[0] != 0x16 {
        return TlsParseResult::NotClientHello;
    }
    if input[1] != 0x03 || !(0x01..=0x04).contains(&input[2]) {
        return TlsParseResult::Invalid(TlsParseError::UnsupportedRecordVersion {
            major: input[1],
            minor: input[2],
        });
    }

    let record_len = u16::from_be_bytes([input[3], input[4]]) as usize;
    if record_len > MAX_TLS_RECORD_BODY {
        return TlsParseResult::Invalid(TlsParseError::RecordTooLarge(record_len));
    }
    let record_total = 5 + record_len;
    if input.len() < record_total {
        return TlsParseResult::NeedMoreData {
            required_total: record_total,
        };
    }

    let record = &input[5..record_total];
    if record.len() < 4 {
        return TlsParseResult::Invalid(TlsParseError::InvalidField("handshake header"));
    }
    if record[0] != 0x01 {
        return TlsParseResult::NotClientHello;
    }

    let handshake_len =
        ((record[1] as usize) << 16) | ((record[2] as usize) << 8) | (record[3] as usize);

    if handshake_len > MAX_TLS_RECORD_BODY - 4 {
        return TlsParseResult::Invalid(TlsParseError::HandshakeTooLarge(handshake_len));
    }
    if 4 + handshake_len > record.len() {
        return TlsParseResult::Invalid(TlsParseError::HandshakeSpansMultipleRecords);
    }

    parse_complete_client_hello(&record[4..4 + handshake_len])
}

fn parse_complete_client_hello(input: &[u8]) -> TlsParseResult {
    let hello_end = input.len();
    let mut offset = 0;

    if let Err(e) = take(input, &mut offset, 2, hello_end, "client_version") {
        return TlsParseResult::Invalid(e);
    }
    if let Err(e) = take(input, &mut offset, 32, hello_end, "random") {
        return TlsParseResult::Invalid(e);
    }

    let session_id_len = match read_u8(input, &mut offset, hello_end, "session_id_length") {
        Ok(v) => v as usize,
        Err(e) => return TlsParseResult::Invalid(e),
    };
    if let Err(e) = take(input, &mut offset, session_id_len, hello_end, "session_id") {
        return TlsParseResult::Invalid(e);
    }

    let cipher_suites_len = match read_be_u16(input, &mut offset, hello_end, "cipher_suites_length")
    {
        Ok(v) => v as usize,
        Err(e) => return TlsParseResult::Invalid(e),
    };
    if cipher_suites_len < 2 || cipher_suites_len % 2 != 0 {
        return TlsParseResult::Invalid(TlsParseError::InvalidField("cipher_suites_length"));
    }
    if let Err(e) = take(
        input,
        &mut offset,
        cipher_suites_len,
        hello_end,
        "cipher_suites",
    ) {
        return TlsParseResult::Invalid(e);
    }

    let compression_len = match read_u8(input, &mut offset, hello_end, "compression_methods_length")
    {
        Ok(v) => v as usize,
        Err(e) => return TlsParseResult::Invalid(e),
    };
    if compression_len == 0 {
        return TlsParseResult::Invalid(TlsParseError::InvalidField("compression_methods_length"));
    }
    if let Err(e) = take(
        input,
        &mut offset,
        compression_len,
        hello_end,
        "compression_methods",
    ) {
        return TlsParseResult::Invalid(e);
    }

    if offset == hello_end {
        return TlsParseResult::ClientHelloWithoutSni;
    }

    let extensions_len = match read_be_u16(input, &mut offset, hello_end, "extensions_length") {
        Ok(v) => v as usize,
        Err(e) => return TlsParseResult::Invalid(e),
    };

    if offset + extensions_len != hello_end {
        return TlsParseResult::Invalid(TlsParseError::InvalidField("extensions_length"));
    }

    let extensions_end = offset + extensions_len;
    let mut extensions_parsed = 0;

    while offset < extensions_end {
        if extensions_end - offset < 4 {
            return TlsParseResult::Invalid(TlsParseError::InvalidField("extension header"));
        }
        extensions_parsed += 1;
        if extensions_parsed > 128 {
            return TlsParseResult::Invalid(TlsParseError::TooManyExtensions);
        }

        let ext_type = match read_be_u16(input, &mut offset, extensions_end, "extension type") {
            Ok(v) => v,
            Err(e) => return TlsParseResult::Invalid(e),
        };
        let ext_len = match read_be_u16(input, &mut offset, extensions_end, "extension length") {
            Ok(v) => v as usize,
            Err(e) => return TlsParseResult::Invalid(e),
        };

        if offset + ext_len > extensions_end {
            return TlsParseResult::Invalid(TlsParseError::InvalidField("extension length"));
        }

        if ext_type == 0 {
            return parse_sni_extension(&input[offset..offset + ext_len]);
        }

        offset += ext_len;
    }

    TlsParseResult::ClientHelloWithoutSni
}

fn parse_sni_extension(data: &[u8]) -> TlsParseResult {
    if data.len() < 2 {
        return TlsParseResult::Invalid(TlsParseError::InvalidSniExtension);
    }

    let list_len = u16::from_be_bytes([data[0], data[1]]) as usize;
    if 2 + list_len != data.len() {
        return TlsParseResult::Invalid(TlsParseError::InvalidSniExtension);
    }

    let mut offset = 2;
    let list_end = 2 + list_len;

    while offset < list_end {
        if list_end - offset < 3 {
            return TlsParseResult::Invalid(TlsParseError::InvalidSniExtension);
        }

        let name_type = data[offset];
        offset += 1;

        let name_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
        offset += 2;

        if offset + name_len > list_end {
            return TlsParseResult::Invalid(TlsParseError::InvalidSniExtension);
        }

        if name_type == 0 && name_len > 0 {
            let hostname = &data[offset..offset + name_len];
            match normalize_sni(hostname) {
                Ok(name) => return TlsParseResult::Sni(name),
                Err(_) => {
                    return TlsParseResult::Invalid(TlsParseError::InvalidHostname);
                }
            }
        }

        offset += name_len;
    }

    TlsParseResult::ClientHelloWithoutSni
}

pub fn normalize_sni(raw: &[u8]) -> Result<String, TlsParseError> {
    let s = std::str::from_utf8(raw).map_err(|_| TlsParseError::InvalidHostname)?;

    if !s.is_ascii() {
        return Err(TlsParseError::InvalidHostname);
    }

    let mut result = s.to_ascii_lowercase();

    if result.ends_with('.') {
        result.pop();
    }

    if result.is_empty() || result.len() > 253 {
        return Err(TlsParseError::InvalidHostname);
    }

    if result.parse::<std::net::IpAddr>().is_ok() {
        return Err(TlsParseError::InvalidHostname);
    }

    for label in result.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(TlsParseError::InvalidHostname);
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(TlsParseError::InvalidHostname);
        }
        for c in label.chars() {
            if !c.is_ascii_alphanumeric() && c != '-' {
                return Err(TlsParseError::InvalidHostname);
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_com() {
        let payload: [u8; 72] = [
            0x16, 0x03, 0x01, 0x00, 0x43, 0x01, 0x00, 0x00, 0x3f, 0x03, 0x03, 0, 1, 2, 3, 4, 5, 6,
            7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
            29, 30, 31, 0x00, 0x00, 0x02, 0x13, 0x01, 0x01, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x0e, 0x00, 0x00, 0x0b, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.',
            b'c', b'o', b'm',
        ];

        let result = parse_client_hello_sni(&payload);
        assert_eq!(result, TlsParseResult::Sni("example.com".to_string()));
    }

    #[test]
    fn truncated_record_requests_exact_total() {
        let payload: [u8; 66] = [
            0x16, 0x03, 0x01, 0x00, 0x43, 0x01, 0x00, 0x00, 0x3f, 0x03, 0x03, 0, 1, 2, 3, 4, 5, 6,
            7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
            29, 30, 31, 0x00, 0x00, 0x02, 0x13, 0x01, 0x01, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x0e, 0x00, 0x00, 0x0b, b'e', b'x', b'a', b'm', b'p',
        ];

        let result = parse_client_hello_sni(&payload);
        assert_eq!(result, TlsParseResult::NeedMoreData { required_total: 72 });
    }

    #[test]
    fn non_handshake_is_not_client_hello() {
        let payload = [0x17, 0x03, 0x01, 0x00, 0x05];
        let result = parse_client_hello_sni(&payload);
        assert_eq!(result, TlsParseResult::NotClientHello);
    }

    #[test]
    fn client_hello_without_sni_is_reported() {
        let payload: [u8; 52] = [
            0x16, 0x03, 0x01, 0x00, 0x2f, 0x01, 0x00, 0x00, 0x2b, 0x03, 0x03, 0, 1, 2, 3, 4, 5, 6,
            7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
            29, 30, 31, 0x00, 0x00, 0x02, 0x13, 0x01, 0x01, 0x00, 0x00, 0x00,
        ];

        let result = parse_client_hello_sni(&payload);
        assert_eq!(result, TlsParseResult::ClientHelloWithoutSni);
    }

    #[test]
    fn rejects_invalid_hostname() {
        let raw = b"bad host.example";
        let result = normalize_sni(raw);
        assert!(matches!(result, Err(TlsParseError::InvalidHostname)));
    }

    #[test]
    fn rejects_too_many_extensions() {
        let mut input = Vec::new();
        input.extend_from_slice(&[
            0x16, 0x03, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x03,
        ]);
        input.extend_from_slice(&[0; 32]);
        input.push(0x00);
        input.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
        input.push(0x01);
        input.push(0x00);

        let extensions_len = 129 * 4;
        input.extend_from_slice(&(extensions_len as u16).to_be_bytes());
        for _ in 0..129 {
            input.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]);
        }

        let record_len = input.len() - 5;
        input[3] = (record_len >> 8) as u8;
        input[4] = record_len as u8;

        let handshake_len = input.len() - 9;
        input[6] = (handshake_len >> 16) as u8;
        input[7] = (handshake_len >> 8) as u8;
        input[8] = handshake_len as u8;

        let result = parse_client_hello_sni(&input);
        assert!(matches!(
            result,
            TlsParseResult::Invalid(TlsParseError::TooManyExtensions)
        ));
    }
}
