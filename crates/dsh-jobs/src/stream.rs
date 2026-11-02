//! Output selection for `job_output`.

use serde::Deserialize;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Most bytes read from one stream file.
pub const MAX_STREAM_BYTES: u64 = 65_536;

/// Which captured stream a caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
}

pub fn read_stream(path: &Path) -> io::Result<String> {
    let mut buffer = Vec::new();
    File::open(path)?
        .take(MAX_STREAM_BYTES)
        .read_to_end(&mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// The requested stream alone, or both streams with labels.
#[must_use]
pub fn format_output(stdout: &str, stderr: &str, stream: Option<Stream>) -> String {
    match stream {
        Some(Stream::Stdout) => stdout.to_string(),
        Some(Stream::Stderr) => stderr.to_string(),
        None => format!("stdout:\n{stdout}\nstderr:\n{stderr}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;

    #[test]
    fn streams_are_capped_at_64_kib_from_the_front() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("big.out");
        fs::write(&path, "a".repeat(70_000)).expect("write");
        let output = read_stream(&path).expect("read");
        assert_eq!(
            output.len(),
            usize::try_from(MAX_STREAM_BYTES).expect("cap")
        );
        assert!(output.bytes().all(|byte| byte == b'a'));
    }

    #[test]
    fn a_missing_stream_file_is_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        assert!(read_stream(&directory.path().join("missing.out")).is_err());
    }

    #[test]
    fn invalid_utf8_is_replaced_not_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("raw.out");
        fs::write(&path, [b'o', 0xff, b'k']).expect("write");
        assert_eq!(read_stream(&path).expect("read"), "o\u{fffd}k");
    }

    proptest! {
        #[test]
        fn selected_streams_are_returned_verbatim(stdout in ".{0,64}", stderr in ".{0,64}") {
            prop_assert_eq!(format_output(&stdout, &stderr, Some(Stream::Stdout)), stdout.as_str());
            prop_assert_eq!(format_output(&stdout, &stderr, Some(Stream::Stderr)), stderr.as_str());
        }

        #[test]
        fn combined_output_labels_and_orders_both_streams(stdout in ".{0,64}", stderr in ".{0,64}") {
            let combined = format_output(&stdout, &stderr, None);
            prop_assert!(combined.starts_with("stdout:\n"));
            prop_assert!(combined.contains("\nstderr:\n"));
            prop_assert!(combined.ends_with(&stderr));
            prop_assert_eq!(combined, format!("stdout:\n{stdout}\nstderr:\n{stderr}"));
        }
    }
}
