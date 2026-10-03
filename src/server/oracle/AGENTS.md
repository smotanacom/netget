# Oracle Server Implementation

**Status:** Planned (Not Yet Implemented)
**Complexity:** 🟠 **Hard**
**Protocol:** Oracle TNS/TTC (Transparent Network Substrate / Two-Task Common)
**Port:** 1521 (default)

---

## Overview

This document describes the planned implementation of an Oracle database server that accepts TNS connections and allows an LLM to control all SQL query responses.

**Critical Context:** This is a **simplified TNS honeypot**, not a full Oracle database implementation. It provides enough protocol support for SQL testing and LLM-controlled responses, but cannot replace a production Oracle database.

---

## Key Hardship: No Rust Server Library Exists

### The Challenge

Unlike MySQL (`opensrv-mysql`) and PostgreSQL (`pgwire`), **there is no Rust library for implementing an Oracle database server**. The Oracle TNS/TTC protocol is:

1. **Proprietary** - Oracle Corporation's closed protocol
2. **Undocumented** - Limited public specification (reverse-engineered only)
3. **Complex** - Two protocol layers (TNS framing + TTC application)
4. **Binary** - Not text-based like PostgreSQL wire protocol
5. **Stateful** - Connection state, cursors, prepared statements, sessions

### Why This Matters

**Other database servers in NetGet:**
- **MySQL:** Uses `opensrv-mysql` v0.8 (~800 LOC total implementation)
- **PostgreSQL:** Uses `pgwire` v0.26 (~900 LOC total implementation)
- **Redis:** Uses `redis-protocol` for parsing only, manual RESP2 encoding (~1,200 LOC)

**Oracle server:**
- **No library available** - Must implement TNS + TTC manually
- **Estimated ~1,700 LOC** (2x Redis complexity)
- **Much higher implementation risk**

---

## Implementation Strategy: Simplified TNS Honeypot

### Design Decisions

Given the complexity and lack of libraries, we implement a **pragmatic subset** of Oracle protocol:

**What We Implement:**
- ✅ **TNS Connection Handshake** (Connect → Accept)
- ✅ **Basic TTC Parsing** (extract SQL queries from Data packets)
- ✅ **SQL Response Encoding** (SELECT result sets, DML OK, errors)
- ✅ **No Authentication** (accept all connections, like MySQL/PostgreSQL servers)
- ✅ **No Storage** (LLM provides all data via memory/instruction)

**What We DON'T Implement:**
- ❌ PL/SQL support (procedures, functions, packages, triggers)
- ❌ Advanced types (CLOB/BLOB streaming, REF CURSOR, XMLTYPE, nested tables)
- ❌ Prepared statements (bind variables)
- ❌ Multiple result sets / cursor management
- ❌ Oracle-specific features (sequences, synonyms, database links, materialized views)
- ❌ Authentication (username/password validation)
- ❌ Row-level security / Virtual Private Database
- ❌ Oracle Real Application Clusters (RAC) support

### Why This Approach Works

**NetGet Pattern:** Database servers provide **LLM-controlled responses**, not actual databases.

- **MySQL server:** No authentication, no storage, LLM answers all queries
- **PostgreSQL server:** No authentication, no storage, LLM answers all queries
- **Oracle server:** Same pattern - LLM is the "database"

**Use Case:** Testing, prototyping, LLM-powered SQL interactions, not production Oracle compatibility.

---

## TNS Protocol (Transparent Network Substrate)

### Packet Structure

Every TNS packet has an **8-byte header**:

```
Offset | Size | Field
-------|------|-------
0      | 2    | Packet Length (big-endian, includes header)
2      | 2    | Packet Checksum (usually 0x0000 = disabled)
4      | 1    | Packet Type
5      | 3    | Flags/Reserved
8      | var  | Payload (depends on packet type)
```

### Packet Types We Handle

| Type | Name | Direction | Purpose |
|------|------|-----------|---------|
| 1 | Connect | Client → Server | Initial connection request |
| 2 | Accept | Server → Client | Connection accepted |
| 3 | Ack | Bidirectional | Acknowledgment |
| 4 | Refuse | Server → Client | Connection refused |
| 6 | Data | Bidirectional | SQL queries and results (contains TTC) |
| 9 | Abort | Bidirectional | Terminate connection |

### Simplified TNS Handshake

```
Client                          Server
  |                               |
  |------- Connect (Type 1) ----->|
  |                               |
  |<------ Accept (Type 2) -------|
  |                               |
  |------- Data (Type 6) -------->| (SQL query in TTC format)
  |                               |
  |<------ Data (Type 6) ---------| (Result set in TTC format)
  |                               |
```

**Hardship:** TNS checksum is typically disabled (0x0000), but some clients may enable it. Our implementation ignores checksums for simplicity.

---

## TTC Protocol (Two-Task Common)

### The Real Challenge

TNS is just the framing layer. The **actual SQL communication** happens via **TTC** (Two-Task Common), which is:

- **Extremely complex** - Oracle's application-layer protocol
- **Poorly documented** - Reverse-engineered from Oracle clients/servers
- **Stateful** - Manages cursors, prepared statements, LOB locators
- **Binary encoded** - Various encoding schemes for different data types

### Simplified TTC Implementation

We implement a **minimal TTC parser/encoder** that:

1. **Extracts SQL** from TTC Data packets (simplified text extraction)
2. **Encodes result sets** (columns + rows as simplified binary format)
3. **Encodes OK responses** (rows affected count)
4. **Encodes errors** (ORA-XXXXX error codes + messages)

**Hardship:** Real TTC is FAR more complex. Our approach:

- **SQL Extraction:** Search for ASCII text in TTC payload (heuristic, not robust)
- **Type Encoding:** All values as strings (simplified, not Oracle's native NUMBER encoding)
- **Column Metadata:** Minimal (name + type code only, no precision/scale)
- **No Fetch Continuation:** All rows returned at once (no paging)

**Why This Works for NetGet:**
- LLM generates SQL responses from memory (doesn't need actual Oracle types)
- Small result sets fit in memory (NetGet is for testing, not production data)
- Clients expecting exact Oracle behavior may fail, but `rust-oracle` client (our test client) is lenient

---

## Library Choices

### Server Libraries: NONE

**Key Hardship:** No Rust crate provides Oracle server functionality.

**Considered Alternatives:**
1. ❌ **Wrapper around Oracle Database XE** - Requires actual Oracle installation (massive overhead)
2. ❌ **Use Java JDBC Thin driver source** - Requires FFI to Java (not practical)
3. ❌ **Port Python oracledb** - Huge undertaking, Oracle's C library dependency
4. ✅ **Manual TNS/TTC implementation** - Pragmatic for NetGet's use case

### Parsing/Encoding: Manual

**No existing crates for:**
- TNS packet parsing/encoding
- TTC protocol parsing/encoding
- Oracle data type encoding (NUMBER, DATE, TIMESTAMP, etc.)

**All of this must be written from scratch.**

---

## LLM Integration

### Query Flow

```
1. Client sends TNS Data packet with TTC payload
2. Server extracts SQL from TTC (simplified parsing)
3. Server calls LLM with oracle_query event:
   {
     "event_type": "oracle_query",
     "event_data": {
       "query": "SELECT employee_id, first_name FROM employees",
       "connection_id": 123
     }
   }
4. LLM returns action:
   {
     "type": "oracle_query_response",
     "columns": [
       {"name": "EMPLOYEE_ID", "type": "NUMBER"},
       {"name": "FIRST_NAME", "type": "VARCHAR2"}
     ],
     "rows": [
       [100, "Steven"],
       [101, "Neena"]
     ]
   }
5. Server encodes response as TTC payload
6. Server wraps TTC in TNS Data packet
7. Server sends to client
```

### LLM Control Points

The LLM controls:
- ✅ **Query responses** (SELECT result sets with columns + rows)
- ✅ **DML responses** (INSERT/UPDATE/DELETE rows affected)
- ✅ **Error responses** (ORA-XXXXX codes with custom messages)
- ✅ **Schema simulation** (table existence, column types via responses)
- ✅ **Data generation** (LLM "remembers" what data exists via instruction/memory)

**No Storage Layer:** Unlike a real Oracle database, we store nothing. The LLM's memory IS the database.

---

## Actions (Server)

### oracle_query_response

**Purpose:** Return SELECT query result set

**Parameters:**
- `columns`: Array of `{name: string, type: string}` (e.g., `[{name: "EMPLOYEE_ID", type: "NUMBER"}]`)
- `rows`: 2D array of values (e.g., `[[100, "Steven"], [101, "Neena"]]`)

**Example:**
```json
{
  "type": "oracle_query_response",
  "columns": [
    {"name": "EMPLOYEE_ID", "type": "NUMBER"},
    {"name": "FIRST_NAME", "type": "VARCHAR2"},
    {"name": "HIRE_DATE", "type": "DATE"}
  ],
  "rows": [
    [100, "Steven", "2003-06-17"],
    [101, "Neena", "2005-09-21"],
    [102, "Lex", "2001-01-13"]
  ]
}
```

**Oracle Type Codes:**
- `VARCHAR2` → Type code 1
- `NUMBER` → Type code 2
- `DATE` → Type code 12
- `CHAR` → Type code 96
- `CLOB` → Type code 112
- `TIMESTAMP` → Type code 180

### oracle_ok_response

**Purpose:** Acknowledge DML statement (INSERT/UPDATE/DELETE)

**Parameters:**
- `rows_affected`: Number (e.g., `5` means 5 rows inserted/updated/deleted)

**Example:**
```json
{
  "type": "oracle_ok_response",
  "rows_affected": 3
}
```

### oracle_error_response

**Purpose:** Return Oracle error

**Parameters:**
- `error_code`: Oracle error number (e.g., `942` for ORA-00942)
- `message`: Error message string

**Example:**
```json
{
  "type": "oracle_error_response",
  "error_code": 942,
  "message": "table or view does not exist"
}
```

**Common Oracle Error Codes:**
- `ORA-00942`: table or view does not exist
- `ORA-00001`: unique constraint violated
- `ORA-01400`: cannot insert NULL into column
- `ORA-02291`: integrity constraint violated - parent key not found
- `ORA-01722`: invalid number

---

## Events (Server)

### oracle_query

**Triggered When:** Client sends SQL query

**Event Data:**
- `query`: SQL query string (SELECT, INSERT, UPDATE, DELETE, CREATE, etc.)
- `connection_id`: Connection identifier

**Available Actions:**
- `oracle_query_response` - Return result set
- `oracle_ok_response` - Acknowledge DML
- `oracle_error_response` - Return error

**Example:**
```json
{
  "event_type": "oracle_query",
  "event_data": {
    "query": "SELECT * FROM employees WHERE department_id = 50",
    "connection_id": 1
  }
}
```

---

## Data Type Handling

### Simplified Type System

**LLM sends values as JSON types:**
- Strings: `"Steven"`, `"2025-11-20"`
- Numbers: `100`, `8000.50`
- Nulls: `null`

**Server encodes to Oracle wire format:**
- All values encoded as **length-prefixed strings** (simplified)
- NUMBER: Convert JSON number to string (e.g., `123.45` → `"123.45"`)
- VARCHAR2: Use string directly
- DATE: Expect string in `YYYY-MM-DD` format
- NULL: Send 0x00 byte (null marker)

**Hardship:** Real Oracle uses complex binary encodings:
- **NUMBER:** Base-100 exponent notation (not decimal strings)
- **DATE:** 7-byte format (century, year, month, day, hour, minute, second)
- **TIMESTAMP:** Extended DATE with fractional seconds
- **ROWIDs:** 10-byte binary format

Our simplified approach works because:
- `rust-oracle` client is lenient (accepts string representations)
- LLM doesn't need to understand binary encodings
- NetGet is for testing, not production Oracle compatibility

---

## File Structure

```
src/server/oracle/
├── mod.rs              # Server connection logic, TNS listener
├── actions.rs          # Protocol + Server trait impl, events, actions
├── tns.rs              # TNS packet parsing/encoding
├── ttc.rs              # TTC parsing/encoding (simplified)
└── CLAUDE.md           # This file
```

---

## Testing Strategy

### Approach

Use real `rust-oracle` client crate to connect and execute queries.

**Mock LLM responses** for predictable testing.

### Test Scenarios (< 10 LLM calls total)

1. **Server Startup** (1 call) - Start Oracle listener on port 1521
2. **SELECT Query** (1 call) - Mock returns result set with 3 rows
3. **INSERT Statement** (1 call) - Mock returns 1 row affected
4. **Error Response** (1 call) - Mock returns ORA-00942 (table not found)
5. **Multiple Queries** (2 calls) - Reuse connection, execute 2 queries
6. **Disconnect** (0 calls) - Client closes connection cleanly

**Total:** ~6 LLM calls (well under 10 budget)

### Mock Example

```rust
let config = NetGetConfig::new("Start Oracle server on port {AVAILABLE_PORT}")
    .with_mock(|mock| {
        mock
            .on_event("oracle_query")
            .and_event_data_contains("query", "SELECT * FROM employees")
            .respond_with_actions(vec![
                json!({
                    "type": "oracle_query_response",
                    "columns": [
                        {"name": "EMPLOYEE_ID", "type": "NUMBER"},
                        {"name": "FIRST_NAME", "type": "VARCHAR2"},
                        {"name": "SALARY", "type": "NUMBER"}
                    ],
                    "rows": [
                        [100, "Steven", 24000],
                        [101, "Neena", 17000],
                        [102, "Lex", 17000]
                    ]
                })
            ])
            .expect_calls(1)
            .and()
    });

// Connect with rust-oracle client
let conn = oracle::Connection::connect("user", "pass", "localhost:1521/XE")?;
let rows = conn.query("SELECT * FROM employees", &[])?;

// Verify results
assert_eq!(rows.len(), 3);
assert_eq!(rows[0].get::<usize, String>(1)?, "Steven");
```

### E2E Test Requirements

- ✅ Localhost only (127.0.0.1)
- ✅ Use `{AVAILABLE_PORT}` placeholder for dynamic port allocation
- ✅ Feature-gated: `#[cfg(all(test, feature = "oracle"))]`
- ✅ Mock LLM responses (default mode)
- ✅ Optional real Ollama: `./test-e2e.sh --use-ollama oracle`
- ✅ Call `.verify_mocks().await?` to ensure all expectations met

---

## Known Limitations

### Protocol Limitations

1. **Simplified TTC Parsing**
   - SQL extraction is heuristic (searches for ASCII text)
   - May fail on complex queries with embedded binary data
   - No support for multi-statement batches

2. **No Prepared Statements**
   - Cannot parse bind variable placeholders (`:1`, `:employee_id`)
   - All queries must be complete SQL strings
   - No bind parameter type negotiation

3. **No PL/SQL Support**
   - Cannot execute stored procedures
   - No anonymous blocks (`BEGIN ... END;`)
   - No packages, functions, or triggers

4. **Basic Type System**
   - Only NUMBER, VARCHAR2, DATE, CHAR, TIMESTAMP supported
   - No CLOB/BLOB streaming
   - No REF CURSOR
   - No user-defined types (objects, VARRAYs, nested tables)

5. **No Authentication**
   - Username/password ignored (all connections accepted)
   - No role-based access control
   - No Oracle user/session management

6. **No Transaction Isolation**
   - COMMIT/ROLLBACK are no-ops (LLM memory is the "transaction")
   - No read consistency
   - No undo/redo management

### Behavioral Differences from Real Oracle

| Feature | Real Oracle | NetGet Oracle Server |
|---------|-------------|---------------------|
| Authentication | Required | Ignored (accept all) |
| Storage | Persistent database | LLM memory only |
| PL/SQL | Full support | Not supported |
| Prepared statements | Supported | Not supported |
| CLOB/BLOB | Streaming | Not supported |
| Transactions | ACID-compliant | No-op (LLM memory) |
| Parallel query | Supported | Not applicable |
| Partitioning | Supported | Not applicable |

### Client Compatibility

**Works With:**
- ✅ `rust-oracle` crate (our test client)
- ✅ Basic SQL clients expecting simple queries

**May Fail With:**
- ❌ Oracle SQL*Plus (expects full protocol compliance)
- ❌ Oracle SQL Developer (uses advanced protocol features)
- ❌ Applications using prepared statements
- ❌ Applications using PL/SQL
- ❌ Applications expecting CLOB/BLOB support

---

## Implementation Complexity

### Lines of Code Estimate

| Component | LOC | Notes |
|-----------|-----|-------|
| tns.rs | 200-300 | TNS packet parsing/encoding |
| ttc.rs | 400-600 | TTC parsing/encoding (simplified) |
| mod.rs | 200-300 | Server handler, connection management |
| actions.rs | 150-200 | Event/action definitions, trait impls |
| **TOTAL** | **950-1,400** | Server implementation only |

**Comparison:**
- MySQL server: ~800 LOC (uses library)
- PostgreSQL server: ~900 LOC (uses library)
- Redis server: ~1,200 LOC (manual RESP2)
- **Oracle server: ~1,200 LOC** (manual TNS/TTC)

### Development Timeline

**Estimated:** 3-5 days for MVP

1. **Day 1:** TNS packet parsing/encoding (tns.rs)
   - Implement packet structure
   - Connect/Accept handshake
   - Data packet wrapping

2. **Day 2:** TTC simplified implementation (ttc.rs)
   - SQL extraction heuristic
   - Result set encoding (columns + rows)
   - OK response encoding
   - Error response encoding

3. **Day 3:** Server handler + LLM integration (mod.rs, actions.rs)
   - TCP listener on port 1521
   - Connection state management
   - LLM event/action flow
   - Action execution

4. **Day 4:** Testing
   - E2E tests with rust-oracle client
   - Mock LLM responses
   - Error handling

5. **Day 5:** Bug fixes + documentation
   - Fix edge cases discovered in testing
   - Complete CLAUDE.md
   - Update docs/archive/ORACLE_PROTOCOL_PLAN.md if needed

---

## Risks & Mitigation

### Risk 1: TTC Parsing Too Complex

**Risk:** Real TTC may be too complex to parse even for simple SQL

**Probability:** Medium
**Impact:** High

**Mitigation:**
- Start with simplest possible heuristic (search for SELECT/INSERT/UPDATE keywords)
- Use `rust-oracle` client for testing (known to work)
- Accept "good enough" for NetGet's testing use case
- Fallback: Implement "echo server" that returns static data if parsing fails

### Risk 2: Client Compatibility

**Risk:** `rust-oracle` client may require features we don't implement

**Probability:** Low
**Impact:** High

**Mitigation:**
- Test early with `rust-oracle` (day 1 of testing)
- Implement minimal required features only
- Document incompatibilities clearly
- Use NetGet Oracle server for testing only (not production)

### Risk 3: Performance Issues

**Risk:** Simplified encoding may be slow for large result sets

**Probability:** Medium
**Impact:** Low

**Mitigation:**
- Limit result sets in tests (< 100 rows)
- Document performance expectations
- Optimize later if needed (post-MVP)

### Risk 4: Debugging Difficulty

**Risk:** Binary protocol makes debugging hard (can't see SQL in Wireshark)

**Probability:** High
**Impact:** Medium

**Mitigation:**
- Add extensive logging (debug level)
- Log all parsed SQL queries
- Log TNS packet types received/sent
- Use hex dumps for debugging TTC payloads

---

## Future Enhancements (Post-MVP)

### Priority 1: Better TTC Parsing

- Study Oracle JDBC Thin driver source (Java)
- Implement proper TTC frame parsing
- Support bind variables (prepared statements)

### Priority 2: PL/SQL Support

- Parse `BEGIN ... END` blocks
- Execute via LLM (treat as special query)
- Return PL/SQL success/failure

### Priority 3: Advanced Types

- CLOB: Return long strings (no streaming, all in memory)
- REF CURSOR: Return as nested result set
- TIMESTAMP: Proper timestamp encoding

### Priority 4: Authentication

- Parse username/password from Connect packet
- Add LLM event: `oracle_authentication`
- LLM decides accept/reject

### Priority 5: Better Oracle Compatibility

- Study more Oracle client implementations
- Add missing TTC opcodes
- Improve type encoding accuracy

---

## Conclusion

The Oracle server implementation is **significantly harder** than MySQL/PostgreSQL due to:
1. **No Rust library** for server-side Oracle protocol
2. **Proprietary protocol** with limited documentation
3. **Two-layer complexity** (TNS framing + TTC application)

However, NetGet's **LLM-controlled response pattern** makes a simplified implementation viable:
- No need for actual database storage
- No need for production Oracle compliance
- Focus on basic SQL query/response flow

**Key Success Factor:** Accept "good enough" for testing use case, not production Oracle replacement.

---

**Last Updated:** 2025-11-20
**Status:** Planning Complete, Implementation Not Started
**See Also:** `docs/archive/ORACLE_PROTOCOL_PLAN.md` (comprehensive plan)
