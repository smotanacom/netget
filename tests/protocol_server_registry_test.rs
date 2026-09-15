use netget::protocol::server_registry::registry;

#[test]
#[ignore = "This test is ignored by default - run with --ignored to see keyword overlaps"]
fn test_keyword_overlaps() {
    // Build registry and check for overlaps
    let reg = registry();
    let overlaps = reg.get_keyword_overlaps();

    if overlaps.is_empty() {
        println!("✓ No keyword overlaps detected!");
        return;
    }

    // Print all overlaps
    println!("\n{} keyword overlaps detected:\n", overlaps.len());

    for (keyword, protocols) in &overlaps {
        println!("  Keyword '{}' is used by:", keyword);
        for (protocol_name, source) in protocols {
            println!("    - {} ({})", protocol_name, source);
        }
        println!();
    }

    // List all protocols with their keywords
    println!("\n=== All Protocol Keywords ===\n");
    let mut protocol_keywords = Vec::new();
    for (protocol_name, protocol) in reg.all_protocols() {
        let keywords = protocol
            .keywords()
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        protocol_keywords.push((protocol_name.clone(), keywords));
    }

    protocol_keywords.sort_by(|a, b| a.0.cmp(&b.0));
    for (protocol_name, keywords) in &protocol_keywords {
        println!("  {}: {}", protocol_name, keywords);
    }

    // Fail the test if there are overlaps
    assert!(
        overlaps.is_empty(),
        "Found {} keyword overlaps. See above for details.",
        overlaps.len()
    );
}
