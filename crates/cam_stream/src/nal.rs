//! Minimal H.265 NAL inspection of 4-byte length-prefixed access units.
//!
//! retina 0.4.20 only reports IDR pictures as random access points for H.265, so streams with
//! open GOPs (CRA keyframes, x265's default) would never resynchronize. We classify them here.

/// Kind of picture an H.265 access unit carries, from its first VCL NAL unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HevcPicture {
    /// IDR / BLA: decodable on its own, nothing after it references earlier pictures.
    ClosedIrap,
    /// CRA: decodable on its own, but following RASL pictures may reference earlier ones.
    Cra,
    /// RASL_N / RASL_R: undecodable when decoding started at the preceding CRA.
    Rasl,
    Other,
}

pub(crate) fn hevc_picture(au: &[u8]) -> HevcPicture {
    let mut rest = au;
    while rest.len() > 4 {
        let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let Some(nal) = rest.get(4..4 + len) else { break };
        if let Some(&header) = nal.first() {
            let nal_type = (header >> 1) & 0x3f;
            match nal_type {
                8 | 9 => return HevcPicture::Rasl,
                16..=20 => return HevcPicture::ClosedIrap,
                21 => return HevcPicture::Cra,
                22 | 23 => return HevcPicture::ClosedIrap,
                0..=31 => return HevcPicture::Other,
                _ => {} // parameter sets, SEI, AUD...
            }
        }
        rest = &rest[4 + len..];
    }
    HevcPicture::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(types: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for t in types {
            out.extend_from_slice(&3u32.to_be_bytes());
            out.extend_from_slice(&[t << 1, 1, 0xaa]);
        }
        out
    }

    #[test]
    fn classifies_first_vcl() {
        assert_eq!(hevc_picture(&au(&[35, 39, 21])), HevcPicture::Cra);
        assert_eq!(hevc_picture(&au(&[19])), HevcPicture::ClosedIrap);
        assert_eq!(hevc_picture(&au(&[20])), HevcPicture::ClosedIrap);
        assert_eq!(hevc_picture(&au(&[16])), HevcPicture::ClosedIrap);
        assert_eq!(hevc_picture(&au(&[8])), HevcPicture::Rasl);
        assert_eq!(hevc_picture(&au(&[39, 1])), HevcPicture::Other);
        assert_eq!(hevc_picture(&au(&[39])), HevcPicture::Other);
        assert_eq!(hevc_picture(&[]), HevcPicture::Other);
        // Truncated length prefix doesn't panic.
        assert_eq!(hevc_picture(&[0, 0, 0, 9, 42]), HevcPicture::Other);
    }
}
