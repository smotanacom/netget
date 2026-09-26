//! `netget::server::zabbix::wire` — the `ZBXD` header and the `sender data` request, the first
//! bytes an unauthenticated peer puts in front of the Zabbix trapper.
//!
//! Asserted, for any input:
//!
//! 1. `parse_header` never panics, and anything it accepts declares at most `MAX_DATA_BYTES` —
//!    the property the session loop relies on before it allocates for the body.
//! 2. `parse_request` never panics or overflows the stack on the body behind an accepted header
//!    (the `depth_bomb` seed is 65,536 levels of JSON nesting; serde_json's recursion limit is
//!    what stops it), and never returns more than `MAX_ITEMS` values.
//! 3. `read_result` never panics, and reads back exactly what `render_result` wrote.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::zabbix::wire::{
    header_len, parse_header, parse_request, read_result, render_result, Request, MAX_DATA_BYTES,
    MAX_ITEMS,
};

fuzz_target!(|data: &[u8]| {
    let _ = read_result(data);

    if let Ok(header) = parse_header(data) {
        assert!(
            header.data_len <= MAX_DATA_BYTES as u64,
            "accepted an oversize declaration"
        );
        let start = header_len(header.flags);
        let end = data.len().min(start + header.data_len as usize);
        if let Ok(Request::SenderData { items, .. }) = parse_request(&data[start..end]) {
            assert!(items.len() <= MAX_ITEMS, "more values than the bound");
            let total = items.len() as u64;
            let failed = total / 2;
            let packet = render_result(total - failed, failed, total, 0.25);
            assert_eq!(read_result(&packet), Some((total - failed, failed, total)));
        }
    }
});
