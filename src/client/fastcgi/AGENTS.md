# FastCGI client — Experimental, FastCGI 1.0, web-server side

Connects to an application (php-fpm, flup, NetGet's responder) and sends Responder requests
with KEEP_CONN over one connection, reconnecting once when the application has closed it.
`fastcgi_request` builds the params a web server would — GATEWAY_INTERFACE, REQUEST_METHOD,
REQUEST_URI, SCRIPT_NAME, SCRIPT_FILENAME (`document_root` + path), DOCUMENT_ROOT, QUERY_STRING,
CONTENT_TYPE/LENGTH, SERVER_*, REMOTE_ADDR, HTTP_* from headers — with `params` overriding,
then PARAMS and STDIN streams. Output is read until END_REQUEST (STDOUT ≤1 MiB + headers,
STDERR ≤64 KiB) and raised as `fastcgi_response` with the parsed CGI status, headers and body,
stderr, app and protocol status. After `request_timeout_secs` (30) it sends ABORT_REQUEST and
waits 5 s more; the event says `aborted`. `fastcgi_get_values` sends GET_VALUES. Shares
`src/server/fastcgi/record.rs`.
