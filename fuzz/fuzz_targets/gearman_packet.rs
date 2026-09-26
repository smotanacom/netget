//! `netget::server::gearman::wire` — the binary packet header, argument splitting and request
//! parsing, and the admin-line parser: the first bytes an unauthenticated peer puts in front of
//! the Gearman server.
//!
//! Asserted, for any input:
//!
//! 1. `parse_header` never panics, and anything it accepts declares at most
//!    `MAX_PACKET_BYTES` — the property the session loop relies on before it allocates.
//! 2. `parse_request` never panics on the body behind an accepted header (the `nul_bomb` seed is
//!    a body of 65,536 NULs; `split_args` only ever splits on the first `n - 1` of them), and a
//!    parsed submission respects the function-name and unique-id bounds.
//! 3. `read_response` never panics, and a rendered answer for any parsed submission reads back
//!    as the packet it is.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::gearman::actions::Work;
use netget::server::gearman::wire::{
    parse_admin, parse_header, parse_request, read_response, Request, HEADER_LEN,
    MAX_FUNCTION_NAME, MAX_PACKET_BYTES, MAX_UNIQUE, WORK_COMPLETE,
};

fuzz_target!(|data: &[u8]| {
    let _ = read_response(data);
    let _ = parse_admin(&String::from_utf8_lossy(data));

    if let Ok(header) = parse_header(data) {
        assert!(
            header.size as usize <= MAX_PACKET_BYTES,
            "accepted an oversize declaration"
        );
        let body = &data[HEADER_LEN..];
        let body = &body[..body.len().min(header.size as usize)];
        if let Ok(Request::Submit {
            function,
            unique,
            workload,
            ..
        }) = parse_request(&header, body)
        {
            assert!(!function.is_empty() && function.len() <= MAX_FUNCTION_NAME * 3);
            assert!(unique.len() <= MAX_UNIQUE * 3);
            let packet = Work::Complete(workload.clone()).render(b"H:fuzz:1");
            assert_eq!(
                read_response(&packet),
                Some((WORK_COMPLETE, vec![b"H:fuzz:1".to_vec(), workload]))
            );
        }
    }
});
