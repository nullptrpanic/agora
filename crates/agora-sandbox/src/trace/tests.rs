use super::TraceContext;

#[test]
fn trace_context_round_trips_an_xff_style_chain() {
    let trace = TraceContext::parse("root, child").unwrap();

    assert_eq!(trace.encode(), "root, child");
}

#[test]
fn child_trace_preserves_its_ancestors() {
    let parent = TraceContext::parse("root, parent").unwrap();
    let child = parent.child();

    let entries = child.encode();
    let entries = entries.split(", ").collect::<Vec<_>>();
    assert_eq!(&entries[..2], ["root", "parent"]);
    assert_eq!(entries.len(), 3);
}

#[test]
fn child_trace_bounds_the_forwarded_chain() {
    let parent =
        TraceContext::new((0..32).map(|index| format!("trace-{index}")).collect()).unwrap();
    let child = parent.child();

    let entries = child.encode();
    let entries = entries.split(", ").collect::<Vec<_>>();
    assert_eq!(entries.len(), 32);
    assert_eq!(entries[0], "trace-1");
    assert_ne!(entries[31], "trace-31");
}

#[test]
fn trace_context_rejects_unsafe_values() {
    assert!(TraceContext::new(Vec::new()).is_err());
    assert!(TraceContext::new(vec!["trace".to_string(); 33]).is_err());
    assert!(TraceContext::parse("").is_err());
    assert!(TraceContext::parse("root, line\nbreak").is_err());
}
