use super::{decode_prepare_request, encode_prepare_request, frame_length};
use std::path::{Path, PathBuf};

#[test]
fn prepare_request_round_trips_non_utf8_paths() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let path = PathBuf::from(OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]));
    let encoded = encode_prepare_request("token", &path).unwrap();
    let length = frame_length(encoded[..4].try_into().unwrap()).unwrap();
    let decoded = decode_prepare_request(&encoded[4..4 + length]).unwrap();

    assert_eq!(decoded.token, "token");
    assert_eq!(decoded.executable, path);
}

#[test]
fn prepare_request_rejects_empty_inputs() {
    assert!(encode_prepare_request("", Path::new("/bin/sh")).is_err());
    assert!(encode_prepare_request("token", Path::new("")).is_err());
}
