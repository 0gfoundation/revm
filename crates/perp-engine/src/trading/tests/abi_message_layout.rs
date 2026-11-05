const IFACE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/interface.rs"));

/// Sum of the declared field widths for the `Message:` block naming `prefix`.
///
/// The prefix contributes its own byte length; every later `name(N)` contributes `N`. Where the
/// doc also annotates the prefix as `"..."(N)` the two must agree — a free consistency check on
/// the one width a reader is most likely to hand-count wrong.
fn declared_width(prefix: &str) -> usize {
    let needle = format!("\"{prefix}\"");
    let start = IFACE
        .find(&format!("Message: {needle}"))
        .unwrap_or_else(|| panic!("no `Message:` block for {prefix}"));
    // The layout runs to the first line that is not a continuation of the doc comment.
    let block: String = IFACE[start..]
        .lines()
        .take_while(|l| l.contains("||") || l.contains("Message:"))
        .collect::<Vec<_>>()
        .join(" ");

    let mut total = prefix.len();
    let after_prefix = &block[block.find(&needle).unwrap() + needle.len()..];
    if let Some(rest) = after_prefix.strip_prefix('(') {
        let n: usize = rest[..rest.find(')').unwrap()].parse().unwrap();
        assert_eq!(n, prefix.len(), "{prefix}: annotated width disagrees with the literal");
    }
    for seg in after_prefix.split("||") {
        let Some(open) = seg.find('(') else { continue };
        let Some(close) = seg[open..].find(')') else { continue };
        // Skip the prefix's own annotation, already counted.
        if seg[..open].trim().is_empty() {
            continue;
        }
        if let Ok(n) = seg[open + 1..open + close].parse::<usize>() {
            total += n;
        }
    }
    total
}

/// Each documented layout must sum to the message the engine actually signs.
#[test]
fn the_documented_widths_sum_to_the_real_message_lengths() {
    for (prefix, len) in [
        ("perpdex_v1_order", 105usize),
        ("perpdex_v1_cancel", 102),
        ("perpdex_v1_leverage", 80),
        ("perpdex_v1_xfer_to", 71),
        ("perpdex_v1_xfer_from", 73),
        ("perpdex_v1_margintype", 75),
    ] {
        assert_eq!(
            declared_width(prefix),
            len,
            "interface.rs's layout for {prefix} does not sum to the message the engine signs"
        );
    }
}

/// The batch item width the doc quotes must match the digest's `ITEM`, and the ABI-encoded
/// width it contrasts against must match `PLACE_ITEM_ENCODED_LEN`. Both were stale at once
/// after `flags` landed — the doc said 43 and 224 against 44 and 256.
#[test]
fn the_batch_item_widths_match_the_code() {
    assert!(
        IFACE.contains("// 44 bytes per item"),
        "interface.rs's batch item width drifted from the digest's ITEM (44)"
    );
    assert_eq!(
        crate::batch::PLACE_ITEM_ENCODED_LEN,
        256,
        "if this changes, the ABI-encoding contrast in interface.rs moves with it"
    );
}
