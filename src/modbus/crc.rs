//! Modbus RTU CRC-16 (poly 0xA001, init 0xFFFF).

/// Compute Modbus RTU CRC-16 over `data` (does not include CRC bytes).
#[must_use]
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFF_u16;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Append CRC-16 little-endian (low byte first) to `frame`.
pub fn append_crc(frame: &mut Vec<u8>) {
    let crc = crc16(frame);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);
}

/// Validate that the last two bytes are the CRC of the preceding payload.
#[must_use]
pub fn check_crc(frame: &[u8]) -> bool {
    if frame.len() < 3 {
        return false;
    }
    let (body, crc_bytes) = frame.split_at(frame.len() - 2);
    let expected = crc16(body);
    let got = u16::from(crc_bytes[0]) | (u16::from(crc_bytes[1]) << 8);
    expected == got
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_read_coils_request() {
        // Waveshare V3 read 8 coils: 01 01 00 00 00 08 + CRC
        let body = [0x01_u8, 0x01, 0x00, 0x00, 0x00, 0x08];
        let crc = crc16(&body);
        // Modbus CRC-16 of this body is 0xCC3D (LE: 3D CC)
        assert_eq!(crc, 0xCC3D);
        let mut frame = body.to_vec();
        append_crc(&mut frame);
        assert_eq!(frame, [0x01, 0x01, 0x00, 0x00, 0x00, 0x08, 0x3D, 0xCC]);
        assert!(check_crc(&frame));
    }

    #[test]
    fn crc_rejects_corrupt() {
        let mut frame = vec![0x01, 0x01, 0x00, 0x00, 0x00, 0x08, 0x3D, 0xCC];
        assert!(check_crc(&frame));
        frame[3] ^= 0x01;
        assert!(!check_crc(&frame));
    }
}
