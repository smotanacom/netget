# RSS Feed Server Implementation

## Overview

RSS (Really Simple Syndication) feed server implementing RSS 2.0 XML generation served over HTTP. The LLM **dynamically
generates feed content** on every request - no in-memory storage.

**Status**: Beta — `metadata()` says so and this file said Experimental for months. The
evidence is `tests/server/rss/e2e_test.rs`, which parses the served bytes with **feed-rs**, a
second implementation; parsing them back with the `rss` crate (which is what the server
*writes* with) proved only that one crate round-trips through itself. RSS has no session, so
fetch-and-parse is the whole protocol and an independent reader is the strongest evidence the
protocol admits.
**RFC**: RSS 2.0 Specification

## Library Choices

- **rss v2.0** - Rust RSS library for RSS 2.0 XML generation
- **hyper v1.0** - HTTP server (same as HTTP protocol)
- **http-body-util** - Body handling utilities

**Rationale**: The `rss` crate provides excellent RSS 2.0 support with builder patterns for channels and items. Hyper
handles the HTTP layer, while the RSS library focuses purely on XML generation.

## Architecture Decisions

### 1. LLM-Driven Feed Generation (No Storage)

RSS server operates **stateless** - feeds are generated fresh on every request:

- Client makes HTTP GET request (e.g., `/tech-news.xml`)
- Server fires `rss_feed_requested` event to LLM
- LLM responds with `generate_rss_feed` action containing structured feed data
- Server builds RSS XML from LLM-provided JSON
- Returns XML with `Content-Type: application/rss+xml`

**No in-memory storage** - each request is independent. This is similar to the HTTP server pattern.

### 2. Request Flow

```
1. Client → HTTP GET /feed.xml
2. Server → LLM event (rss_feed_requested with path and headers)
3. LLM → generate_rss_feed action with JSON data
4. Server → Build RSS XML from JSON
5. Server → Client with RSS XML
```

### 3. Category Support

Items can have categories in two formats:

**Simple string**:

```json
"categories": ["AI", "Technology", "Science"]
```

Renders as:

```xml
<category>AI</category>
<category>Technology</category>
<category>Science</category>
```

**Object with domain**:

```json
"categories": [
  "AI",
  {"name": "Machine Learning", "domain": "tech.example.com"}
]
```

Renders as:

```xml
<category>AI</category>
<category domain="tech.example.com">Machine Learning</category>
```

### 4. Sync Action Model

RSS uses **sync actions** (not async):

- `generate_rss_feed` - LLM action to generate feed XML
- Returns structured JSON with feed metadata and items
- Server parses JSON and builds RSS XML using `rss` crate

### 5. Dual Logging

All RSS operations use dual logging:

- **INFO**: Feed requests, feed generation
- **DEBUG**: LLM interactions, request details
- Both go to `netget.log` (via tracing) and TUI (via status_tx)

### 6. Error Handling

Four outcomes, deliberately kept apart. Three of them could be 404 and must not be: a reader
told 404 stops polling, so netget failing has to look different from the model deciding there
is no feed here.

| Outcome | Wire | `decision=` tag |
|---|---|---|
| A feed was built | 200 + `application/rss+xml` | `model_feed` |
| The model produced no `generate_rss_feed` | 404 `Feed Not Found` | `model_no_feed` |
| It produced one netget refused (missing `title`/`link`/`description`, non-array `items`) | 500 | `fail_closed_unusable_feed` |
| The backend failed | 503 + `Retry-After` when saturated, else 500 | `fail_closed_llm_error` |
| Not a GET | 405 | — |

The peer gets a `crate::utils::WireFailure` **category**, never the error text; the error goes
to the log. The `decision=` tag is what tells the three failure shapes apart, because the status
code alone cannot — the same rule `src/server/radius/` follows.

A refused action never reaches `protocol_results`, so the handler reads
`ExecutionResult::failures` too. Without that, a `generate_rss_feed` missing its title was
answered 404 — the model's own vocabulary for "no feed here" — and the reason lived only in the
log.

### 7. Model content cannot forge feed structure

The feed is XML built from strings the model wrote, which is the CR/LF-injection class in an XML
costume. It does not work, and `tests/server/rss/injection_test.rs` measures it rather than
assuming it:

- text elements go through `quick_xml`'s `BytesText::new`, which escapes on write;
- an item's `<description>` is CDATA via `BytesCData::escaped`, which **splits** on `]]>` rather
  than escaping it, so the terminator cannot appear inside a section;
- a category `domain` is an attribute, and attribute values are escaped.

What escaping cannot reach is the set of characters XML 1.0 §2.2 forbids outright — there is no
entity for NUL, and one in a title yields a 200 carrying a document every conforming reader
rejects. `xml_safe` in `mod.rs` drops exactly those (keeping tab, LF and CR, which are legal).
Check this again on any `rss`/`quick-xml` bump.

### 8. Required channel fields are refused, not defaulted

`title`, `link` and `description` are declared `required: true` and the executor enforces it.
They used to be read with `unwrap_or("Untitled Feed")` / `unwrap_or("http://localhost")` /
`unwrap_or("No description")`, so an answer naming none of the three produced a
complete-looking feed and nothing recorded that the model had not supplied one — a required
field whose default asserts a result. `items` is required but may be empty: a feed with no
entries is a legitimate answer, a feed with no title is not a feed.

## LLM Integration

### Events

**rss_feed_requested** - Fired when client requests a feed

- Parameters:
    - `path` - Feed path (e.g., `/news.xml`)
    - `headers` - HTTP request headers (object)

### Actions

**generate_rss_feed** (sync action):

```json
{
  "type": "generate_rss_feed",
  "title": "Tech News Feed",
  "link": "https://example.com",
  "description": "Latest technology news",
  "language": "en-us",
  "ttl": "60",
  "last_build_date": "Mon, 09 Nov 2025 12:00:00 GMT",
  "items": [
    {
      "title": "New AI Model Released",
      "link": "https://example.com/ai-news",
      "description": "Company X released new model",
      "author": "editor@example.com (Editor Name)",
      "pub_date": "Mon, 09 Nov 2025 10:00:00 GMT",
      "guid": "https://example.com/ai-news",
      "categories": [
        "AI",
        "Technology",
        {"name": "Machine Learning", "domain": "tech.example.com"}
      ]
    }
  ]
}
```

### Feed Data Structure

**Channel fields** (all strings):

- `title` - Feed title (required)
- `link` - Feed link/website URL (required)
- `description` - Feed description (required)
- `language` - Language code (optional, e.g., "en-us")
- `ttl` - Time to live in minutes (optional)
- `last_build_date` - Last build date in RFC 2822 format (optional)

**Item fields**:

- `title` - Item title (string)
- `link` - Item link/URL (string, optional)
- `description` - Item description/content (string, optional)
- `author` - Author email (RFC 2822 format, optional)
- `pub_date` - Publication date (RFC 2822 format, optional)
- `guid` - Globally unique identifier (string, optional)
- `categories` - Array of strings or objects (optional)

## Known Limitations

### 1. No Persistence

- Feeds generated on every request
- No caching between requests
- LLM must regenerate content each time
- Good: Always fresh, no stale data
- Bad: Higher LLM call volume

### 2. No Feed Discovery

- No index page listing available feeds
- Clients must know feed paths
- No `/` endpoint showing all feeds
- Could add as future enhancement

### 3. No connection-level rate limit

Every GET is one LLM call and nothing bounds the rate, so an unauthenticated fetch loop is an
unauthenticated model-call loop. `src/server/tuntap/` is the protocol in this tree that solves
the equivalent problem (a filter plus a rolling per-minute window); RSS has no such bound.

### 4. No Authentication

- All feeds publicly accessible
- No access control or authentication
- Anyone can read any feed

### 5. No Pagination

- All items returned in single response
- Large feeds may be slow to generate/transmit
- No support for paging or item limits

### 6. No Atom Support

- Only RSS 2.0 format
- No Atom 1.0 feeds
- Could add Atom support via `atom_syndication` crate

### 7. No Conditional Requests

- No If-Modified-Since support (server side)
- No ETag generation
- No 304 Not Modified responses
- Client has If-Modified-Since support though

## Example Prompts

### Basic Feed Server

```
listen on port 8080 via rss
For /news.xml, serve a feed titled "Daily News" with 3 tech news items
Include categories like AI, Cloud, and Quantum for each item
```

### Multiple Feeds

```
start rss server on port 8080
For /tech.xml: "Tech News" feed with 5 items about AI and programming
For /sports.xml: "Sports Daily" feed with 3 items about football and basketball
Use relevant categories for each item
```

### Blog Feed with Metadata

```
rss server on 8080
For /blog.xml: "My Dev Blog"
- Language: en-us
- TTL: 60 minutes
- 3 blog posts about Rust, Python, and Web Development
- Include author field: john@example.com (John Doe)
- Add GUID for each post
- Categories: Programming, Tutorial, etc.
```

## Performance Characteristics

### Latency

- One LLM call per HTTP request
- Typical latency: 2-5 seconds per request with qwen3-coder:30b
- XML generation: <1ms after LLM response
- Total: ~2-5 seconds per feed request

### Throughput

- Limited by LLM response time (2-5s per request)
- Concurrent requests processed in parallel (each on separate tokio task)
- No shared state means no lock contention

### Memory Usage

- No persistent storage - very low memory footprint
- Each request allocates temporarily for XML generation
- Memory freed immediately after response sent

## Comparison with HTTP Server

| Feature            | HTTP                           | RSS                |
|--------------------|--------------------------------|--------------------|
| Request Handling   | LLM per request                | LLM per request    |
| Response Format    | LLM chooses (HTML, JSON, etc.) | Always RSS 2.0 XML |
| Structured Actions | send_http_response             | generate_rss_feed  |
| State              | Stateless                      | Stateless          |
| Categories         | N/A                            | Built-in support   |

Both protocols follow the same pattern: receive request → call LLM → generate response.

## Future Enhancements

### 1. Conditional Requests

Support If-Modified-Since:

- Store last-modified timestamps
- Return 304 Not Modified when appropriate
- Reduce bandwidth for unchanged feeds

### 2. ETag Support

Generate ETags for feeds:

- Hash of feed content
- Enable client caching
- Return 304 when ETag matches

### 3. Atom Support

Support Atom 1.0 format:

- Use `atom_syndication` crate
- Serve both RSS and Atom
- Content negotiation via Accept header

### 4. Feed Index

Add `/` endpoint:

- List all available feeds
- Generate HTML or JSON directory
- Auto-discovery links

### 5. Pagination

Support large feeds:

- Limit items per page
- Add next/prev links
- Query parameters for pagination

### 6. Media Enclosures

Support podcast/media RSS:

- `<enclosure>` tags
- File size and type metadata
- iTunes/Spotify RSS extensions

## References

- [RSS 2.0 Specification](https://www.rssboard.org/rss-specification)
- [rss Crate Documentation](https://docs.rs/rss/latest/rss/)
- [RSS on Wikipedia](https://en.wikipedia.org/wiki/RSS)
- [RSS Best Practices](https://www.rssboard.org/rss-profile)
- [RFC 2822 Date Format](https://datatracker.ietf.org/doc/html/rfc2822#section-3.3)
