use rfb_runtime::resources::RuntimeLimits;

#[test]
fn default_limits_are_valid_and_bounded() {
    let limits = RuntimeLimits::default();
    limits.validate().unwrap();
    assert!(limits.channel_capacity <= 64);
    assert_eq!(
        rfb_runtime::codec::FrameCodec::from_limits(&limits).max_payload,
        limits.max_frame_bytes - 18
    );
    assert!(limits.max_event_bytes <= limits.max_frame_bytes);
}

#[test]
fn limits_reject_unbounded_values() {
    let limits = RuntimeLimits {
        channel_capacity: 0,
        ..RuntimeLimits::default()
    };
    assert!(limits.validate().is_err());
    let limits = RuntimeLimits {
        max_event_bytes: RuntimeLimits::default().max_frame_bytes + 1,
        ..RuntimeLimits::default()
    };
    assert!(limits.validate().is_err());
}
