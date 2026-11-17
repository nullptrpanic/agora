use super::*;

fn fragment(id: &str, count: &str, seq: &str, payload: &[u8]) -> LarkFrame {
    let mut frame = LarkFrame::default();
    frame.upsert_header("message_id", id);
    frame.upsert_header("sum", count);
    frame.upsert_header("seq", seq);
    frame.payload = payload.to_vec();
    frame
}

#[test]
fn fragmented_events_reject_invalid_headers_and_conflicts() {
    for (id, sum, seq) in [
        ("a", "0", "0"),
        ("a", "65", "0"),
        ("a", "2", "2"),
        ("a", "2", "-1"),
        ("a", "bad", "0"),
        ("", "2", "0"),
    ] {
        assert!(
            LarkFragments::default()
                .reassemble(&mut fragment(id, sum, seq, b"part"))
                .is_err()
        );
    }
    let mut cache = LarkFragments::default();
    assert!(
        !cache
            .reassemble(&mut fragment("a", "2", "0", b"first"))
            .unwrap()
    );
    assert!(
        cache
            .reassemble(&mut fragment("a", "2", "0", b"different"))
            .is_err()
    );
    assert!(cache.messages.is_empty());
    assert!(
        !cache
            .reassemble(&mut fragment("a", "2", "0", b"new"))
            .unwrap()
    );
    assert!(
        cache
            .reassemble(&mut fragment("a", "3", "1", b"wrong count"))
            .is_err()
    );
    assert!(cache.messages.is_empty());
    let mut missing_sequence = fragment("a", "2", "0", b"part");
    missing_sequence
        .headers
        .retain(|header| header.key != "seq");
    assert!(cache.reassemble(&mut missing_sequence).is_err());
}

#[test]
fn fragmented_events_expire_and_can_be_retransmitted() {
    let mut cache = LarkFragments::default();
    assert!(
        !cache
            .reassemble(&mut fragment("a", "2", "0", b"old"))
            .unwrap()
    );
    cache.messages.get_mut("a").unwrap().started = Instant::now() - Duration::from_secs(6);
    assert!(
        !cache
            .reassemble(&mut fragment("a", "2", "1", b"tail"))
            .unwrap()
    );
    let mut first = fragment("a", "2", "0", b"new");
    assert!(cache.reassemble(&mut first).unwrap());
    assert_eq!(first.payload, b"newtail");
    assert!(cache.messages.is_empty());
}

#[test]
fn fragmented_events_bound_count_and_total_bytes() {
    let mut cache = LarkFragments::default();
    for index in 0..64 {
        assert!(
            !cache
                .reassemble(&mut fragment(&index.to_string(), "2", "0", b"x"))
                .unwrap()
        );
    }
    assert!(
        cache
            .reassemble(&mut fragment("extra", "2", "0", b"x"))
            .is_err()
    );
    let mut last = fragment("0", "2", "1", b"y");
    assert!(cache.reassemble(&mut last).unwrap());
    assert_eq!(last.payload, b"xy");
    assert!(
        !cache
            .reassemble(&mut fragment("extra", "2", "0", b"x"))
            .unwrap()
    );

    let mut cache = LarkFragments::default();
    assert!(
        !cache
            .reassemble(&mut fragment("a", "2", "0", &vec![1; 1024 * 1024]))
            .unwrap()
    );
    assert!(
        cache
            .reassemble(&mut fragment("b", "2", "0", b"x"))
            .is_err()
    );
    assert!(
        cache
            .reassemble(&mut fragment("a", "2", "1", b"x"))
            .is_err()
    );
    assert!(cache.messages.is_empty());
    assert!(
        cache
            .reassemble(&mut fragment("a", "2", "0", &vec![0; 1024 * 1024 + 1]))
            .is_err()
    );
}
