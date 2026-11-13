use super::*;
#[test]
fn feedback_table_preserves_unknown_format_slots_and_modifiers() {
    let mut bytes = Vec::new();
    for (code, modifier) in [
        (0_u32, 0_u64),
        (Fourcc::Argb8888 as u32, 0x1234_5678_abcd_ef01),
    ] {
        bytes.extend(code.to_ne_bytes());
        bytes.extend(0_u32.to_ne_bytes());
        bytes.extend(modifier.to_ne_bytes());
    }
    let formats = table(&bytes).expect("feedback table");
    assert!(formats[0].is_none());
    assert_eq!(
        u64::from(formats[1].expect("known format").modifier),
        0x1234_5678_abcd_ef01
    );
    assert!(table(&bytes[..31]).is_err());
}

#[test]
#[cfg(not(miri))]
fn feedback_read_ignores_and_preserves_the_shared_fd_position() {
    use std::io::Seek;
    let mut file = tempfile::tempfile().expect("feedback FD");
    file.write_all(&(Fourcc::Argb8888 as u32).to_ne_bytes())
        .expect("format");
    file.write_all(&0_u32.to_ne_bytes()).expect("padding");
    file.write_all(&0_u64.to_ne_bytes()).expect("modifier");
    // The sender has left the shared description at EOF.
    let formats = read_table(file.try_clone().expect("duplicate FD"), 16).expect("feedback table");
    assert_eq!(formats[0].expect("known format").code, Fourcc::Argb8888);
    assert_eq!(file.stream_position().expect("shared position"), 16);
}

#[test]
fn feedback_updates_replace_previous_tranche_indices() {
    let format = Format {
        code: Fourcc::Argb8888,
        modifier: Modifier::Linear,
    };
    let mut host = Host {
        table: vec![None, Some(format)],
        indices: BTreeSet::from([1]),
        ..Host::default()
    };
    host.finish_feedback().expect("first feedback batch");
    assert_eq!(host.formats, [format]);
    host.table = vec![Some(format)];
    host.indices.insert(0);
    host.finish_feedback().expect("replacement feedback batch");
    assert_eq!(host.formats, [format]);
    assert!(host.indices.is_empty());
}
