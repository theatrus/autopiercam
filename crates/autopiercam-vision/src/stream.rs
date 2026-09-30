//! Private local worker protocol. One outstanding request, no frame queue.
//! The caller owns timeout/termination; this process has no camera/network code.
use anyhow::{Result, ensure};
use std::io::{Read, Write};

pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

pub fn read_frame(input: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut length = [0; 4];
    if input.read(&mut length[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut length[1..])?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(
        (1..=MAX_FRAME_BYTES).contains(&length),
        "Invalid frame length"
    );
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    Ok(Some(bytes))
}

pub fn write_response(output: &mut impl Write, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_eof_truncation_and_multiple_frames() {
        assert!(read_frame(&mut &[][..]).unwrap().is_none());
        for bytes in [
            vec![1],
            vec![0; 4],
            u32::MAX.to_le_bytes().to_vec(),
            vec![2, 0, 0, 0, 9],
        ] {
            assert!(read_frame(&mut bytes.as_slice()).is_err());
        }
        let mut frames = &[2, 0, 0, 0, 7, 8, 1, 0, 0, 0, 9][..];
        assert_eq!(read_frame(&mut frames).unwrap(), Some(vec![7, 8]));
        assert_eq!(read_frame(&mut frames).unwrap(), Some(vec![9]));
        assert!(read_frame(&mut frames).unwrap().is_none());
    }
    #[test]
    fn responses_are_one_json_line() {
        let mut bytes = Vec::new();
        write_response(&mut bytes, &serde_json::json!({"error": "line\nbreak"})).unwrap();
        assert_eq!(bytes.iter().filter(|&&v| v == b'\n').count(), 1);
        assert_eq!(*bytes.last().unwrap(), b'\n');
    }
}
