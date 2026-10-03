# RSS E2E Test Strategy

## Overview

E2E tests for RSS feed server that verify LLM-driven dynamic feed generation, category support, and RSS 2.0 compliance.

## Test Approach

**Strategy**: Single comprehensive test with multiple feed fetches
**Client**: reqwest HTTP client + **feed-rs** for parsing
**LLM Calls**: ~6 total (within budget)
**Runtime**: ~10-15 seconds

## Test Coverage

### Test Scenarios

1. **Tech News Feed** (`/tech-news.xml`)
    - 3 items with varying categories
    - Categories include simple strings and objects with domains
    - Verifies structured category rendering
    - Checks feed metadata (title, link, description, language, TTL)

2. **Sports Feed** (`/sports.xml`)
    - 2 items with simple categories
    - Tests different feed structure
    - Verifies proper XML generation

3. **Blog Feed** (`/blog.xml`)
    - 2 items with author and GUID fields
    - Tests optional RSS elements
    - Verifies complex category structures

4. **404 Handling** (`/nonexistent.xml`)
    - Tests non-existent feed path
    - Verifies proper error response

5. **Independent parsing** (the load-bearing one)
    - Parses the served XML with **feed-rs**, not with the `rss` crate
    - `src/server/rss` *generates* its XML with the `rss` crate's `ChannelBuilder`. Parsing it
      back with the same crate proved only that one crate round-trips through itself, which is
      the circular-evidence trap the root CLAUDE.md warns about, and is why RSS sat at
      Experimental with a passing test.
    - feed-rs is a separate implementation with its own normalised model, so what it accepts is
      evidence about the bytes on the wire.
    - Asserts: `FeedType::RSS2`, channel title/description/language, three entries, the first
      entry's title and link, that its RFC 2822 `pub_date` parsed into a real timestamp (feed-rs
      silently drops dates it cannot read, so `published.is_some()` is a real check), and its
      categories.

## LLM Call Budget

**Total: ~6 LLM calls**

Breakdown:

1. Initial server setup (instruction parsing)
2. Tech news feed generation
3. Sports feed generation
4. Blog feed generation
5. Non-existent feed (404 response)
6. Repeat tech feed fetch (for parsing test)

**Optimization**: Single server with comprehensive prompt reduces setup overhead.

## Key Features Tested

✅ **Dynamic feed generation** - LLM generates feeds per request (no memory)
✅ **Category support** - Simple strings and complex objects with domains
✅ **RSS 2.0 compliance** - Valid XML structure
✅ **Optional fields** - Author, GUID, language, TTL, pub_date
✅ **HTTP headers** - Proper Content-Type (application/rss+xml)
✅ **Error handling** - 404 for non-existent feeds
✅ **Parsing validation** - Generated XML parses with feed-rs, an independent implementation

## Runtime Characteristics

- **Build time**: Included in feature-specific build (~10-15s)
- **Test execution**: ~10-15 seconds
    - Server startup: ~2s
    - Each feed fetch: ~1-2s (LLM generation time)
    - Parsing validation: <1s
- **Total**: ~15-20 seconds end-to-end

## Assertions

### HTTP Layer

- Status codes (200 for valid, 404 for invalid)
- Content-Type header (application/rss+xml; charset=utf-8)
- Response body contains valid XML

### RSS Structure

- `<rss version="2.0">` tag present
- Channel elements (title, link, description)
- Item elements with titles and content
- Category tags properly formatted
- Optional elements (author, GUID, language, TTL)

### Content Validation

- Feed titles match specification
- Item counts correct (3, 2, 2 items)
- Categories rendered correctly
    - Simple: `<category>AI</category>`
    - With domain: `<category domain="ai.example.com">Deep Learning</category>`
- Dates in RFC 2822 format
- Links are valid URLs

### Parsing Validation

- feed-rs can parse the generated XML and classifies it as RSS 2.0
- Channel metadata, entry links, dates and categories are all readable through its model
- No parsing errors

## Known Issues

None currently identified.

## Future Enhancements

### Potential Additional Tests

1. **If-Modified-Since header**
    - Test conditional requests
    - Verify 304 Not Modified responses

2. **Feed autodiscovery**
    - Test HTML pages with `<link rel="alternate">` tags
    - Verify feed URLs can be discovered

3. **Large feeds**
    - Test feeds with 100+ items
    - Verify performance and memory usage

4. **Malformed requests**
    - Test with invalid headers
    - Test with non-GET methods

5. **Concurrent requests**
    - Test multiple simultaneous feed fetches
    - Verify LLM handles parallelism

## Running Tests

```bash
# Run RSS E2E tests
./cargo-isolated.sh test --no-default-features --features rss --test server::rss::e2e_test

# With debug output to see LLM interactions
RUST_LOG=debug ./cargo-isolated.sh test --no-default-features --features rss --test server::rss::e2e_test
```

## Dependencies

- **reqwest**: HTTP client for fetching feeds
- **feed-rs** (dev-dependency): independent RSS/Atom parser — the evidence
- **tokio**: Async runtime

Do **not** switch the parsing assertions back to the `rss` crate: it is what the server
serialises with, and the whole point of feed-rs here is that it is not.

## Notes

- Tests use dynamic port allocation (port 0) to avoid conflicts
- Each feed fetch triggers a new LLM call (no caching)
- Categories support both string and object formats
- Feed generation is stateless (no memory between requests)
- Uses debug log level to observe LLM interactions during testing
