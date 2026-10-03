# Git Client Implementation

## Overview

Git client protocol implementation for NetGet, enabling LLM-controlled Git operations including clone, fetch, pull,
push, and repository inspection.

## Library Choices

### Primary Library: git2 (libgit2)

**Crate**: `git2` v0.18+
**Description**: Rust bindings to libgit2, a pure C implementation of Git core methods

**Why git2:**

- Mature and widely used (libgit2 is battle-tested)
- Supports all major Git operations (clone, fetch, pull, push, etc.)
- Handles authentication (username/password, SSH keys, tokens)
- Does not require external `git` binary
- Thread-safe and suitable for async contexts

**Limitations:**

- Requires libgit2 system library (usually available via package managers)
- Some advanced Git features may require manual implementation
- Authentication callback handling can be verbose

### Alternative Considered: gix (gitoxide)

**Crate**: `gix` (pure Rust Git implementation)
**Status**: Not chosen for initial implementation

**Pros:**

- Pure Rust (no C dependencies)
- Modern API design
- Growing ecosystem

**Cons:**

- Less mature than git2/libgit2
- Smaller community and fewer examples
- Some features still under development

**Decision:** Use `git2` for stability and maturity. Consider `gix` for future iterations.

## Architecture

### Connection Model

Unlike network-based clients (TCP, HTTP), Git operations are **file-system based**:

1. **"Connection"** = Client initialization
2. **Operations** are discrete actions (clone, fetch, status, etc.)
3. **No persistent network connection** (operations are request-response)
4. **Repository state** is maintained on the file system

### Dummy Socket Address

Since Git doesn't use network sockets, we return a dummy `SocketAddr` (`127.0.0.1:0`) to satisfy the `Client` trait
requirements. This is a placeholder and doesn't represent an actual network endpoint.

### State Management

**Per-Client State:**

- `repo_path`: Local path to the Git repository
- `remote_url`: URL of the remote repository (for clone/fetch/push)
- `username`/`password`: Authentication credentials

**Startup parameters:** `local_path`, `username`, `password`, `allowed_root`,
`allow_remote_writes` — all five are read by `connect_with_llm_actions`. `local_path` is
opened as the working repository if it already is one, and is the default destination for a
`git_clone` that omits `path`. The last two are the sandbox; see **Security Considerations**.

**They are read from `ConnectContext::startup_params`, not from `protocol_data`, and the
distinction is not cosmetic.** `cli/client_startup.rs` leaves `ClientInstance::protocol_data`
as `Value::Null` and only a client that writes to it itself ever populates it — which this one
does not. So a parameter read *only* through `get_protocol_field` is never actually delivered,
whatever the declaration says, and that was the state of `local_path`, `username` and
`password`: declared, threaded all the way to `Cred::userpass_plaintext`, and always `None`.
`read_field` now consults `startup_params` first and keeps the `protocol_data` lookup as a
second chance. **If you add a startup parameter to any client, check which of the two channels
actually carries it** — the declaration compiling is not evidence that it arrives.

**State Flow:**

1. Client initializes with target (URL or local path)
2. LLM receives `git_connected` event
3. LLM issues actions (clone, fetch, etc.)
4. Each action triggers an event (`git_operation_completed` or `git_operation_error`)
5. LLM responds to events with follow-up actions

### Authentication

Git2 supports multiple authentication methods:

1. **Username/Password** (HTTPS):
   ```rust
   callbacks.credentials(|_url, _username, _allowed| {
       Cred::userpass_plaintext(username, password)
   });
   ```

2. **Personal Access Token** (HTTPS):
    - Use token as password with empty or token username
    - Example: GitHub PATs (`ghp_xxxxxxxxxxxxx`)

3. **SSH Keys** (SSH URLs):
    - Not implemented in initial version
    - Would require `Cred::ssh_key()` or `Cred::ssh_key_from_agent()`

**Current Implementation:** Supports username/password authentication via startup parameters.

## LLM Integration

### Event Flow

```
User Instruction → git_connected event
                 ↓
            LLM decides action (e.g., clone)
                 ↓
            execute_git_action (git_clone)
                 ↓
            git_operation_completed event
                 ↓
            LLM responds (e.g., check status)
                 ↓
            execute_git_action (git_status)
                 ↓
            git_operation_completed event
                 ↓
            ...
```

### Event Types

1. **git_connected** (`repository_path`): client initialized, ready for operations.
2. **git_operation_completed** (`operation`, `detail`, `output`): the verb ran.
   `output` is what it produced — the commit log, the diff, the branch list, the
   status text — truncated to 8 KiB for the prompt.
3. **git_operation_error** (`operation`, `error`): the verb failed.

**Until September 2026 the last two were declared and raised by nothing**, so the
loop drawn above did not exist: the model got one turn on the connected event and
then went deaf. `git_log` reported "12 line(s)" to the operator and threw the log
away, so "show me the last 5 commits" was unanswerable. They were also declared as
throwaway `EventType`s inside `get_event_types()` whose example action was
`{"type": "placeholder"}` — a shape `execute_action` rejects, rendered verbatim
into the documentation the model reads — and whose `git_connected` was a *different*
value from the `GIT_CLIENT_CONNECTED_EVENT` the client actually raises. All three
are now the real statics, and `GitClient::run_and_report` emits the two operation
events for every action the LLM path runs.

**The chain is bounded, not cut.** action → event → action is genuinely
self-referential, so `run_and_report` returns `Pin<Box<dyn Future … + Send>>`
(an `async fn` awaiting itself is E0391, and `+ Send` must be named because this is
awaited inside a `tokio::spawn`) and `MAX_FOLLOWUP_DEPTH = 6` stops a model that
answers `git_status` with `git_status`.

**The injected-command path does not report.** `[ send ]` returns the outcome to
the operator who pressed it; spending LLM budget on their behalf is not what that
button means. Only the LLM path chains.

### Action Types

**Async Actions (User-triggered):**

- `git_clone`: Clone a repository
- `git_fetch`: Fetch updates from remote
- `git_pull`: Pull and merge updates (fast-forward only)
- `git_push`: Push commits to remote
- `git_checkout`: Checkout branch or commit, create new branches
- `git_delete_branch`: Delete local or remote branches
- `git_list_branches`: List local/remote branches
- `git_list_tags`: List all tags in repository
- `git_create_tag`: Create lightweight or annotated tags
- `git_log`: Get commit history
- `git_status`: Get repository status
- `git_diff`: View working directory, staged, or commit differences
- `disconnect`: Close the client

**Sync Actions:**

- None (Git operations are discrete, not response-based)

### LLM Action Examples

```json
{
  "type": "git_clone",
  "url": "https://github.com/user/repo.git",
  "path": "./my-repo"
}

{
  "type": "git_list_branches",
  "remote": true
}

{
  "type": "git_log",
  "max_count": 5
}

{
  "type": "git_status"
}

{
  "type": "git_delete_branch",
  "branch": "feature-branch",
  "force": false,
  "remote": "origin"
}

{
  "type": "git_list_tags"
}

{
  "type": "git_create_tag",
  "name": "v1.0.0",
  "target": "HEAD",
  "message": "Release version 1.0.0"
}

{
  "type": "git_diff",
  "staged": true
}
```

## Implementation Details

### Supported Operations

| Operation         | Status            | Notes                                                  |
|-------------------|-------------------|--------------------------------------------------------|
| **clone**         | ✅ Implemented     | With HTTPS authentication                              |
| **fetch**         | ✅ Implemented     | From named remote                                      |
| **status**        | ✅ Implemented     | Shows modified/untracked files                         |
| **list_branches** | ✅ Implemented     | Local and remote branches                              |
| **log**           | ✅ Implemented     | Commit history with limit                              |
| **pull**          | ✅ Implemented     | Fetch + fast-forward merge, manual merge for conflicts |
| **push**          | ✅ Implemented     | Push commits to remote with authentication             |
| **checkout**      | ✅ Implemented     | Checkout branches/commits, create new branches         |
| **delete_branch** | ✅ Implemented     | Delete local or remote branches with safety checks     |
| **list_tags**     | ✅ Implemented     | List all tags in repository                            |
| **create_tag**    | ✅ Implemented     | Create lightweight or annotated tags                   |
| **diff**          | ✅ Implemented     | View working directory, staged, or commit differences  |
| **commit**        | ❌ Not implemented | Requires staging and commit creation                   |
| **merge**         | ❌ Not implemented | Complex, requires conflict resolution                  |

### Threading Model

Git2 operations are **blocking** but thread-safe:

- Operations run in Tokio tasks (via `tokio::spawn`)
- No explicit async (git2 is synchronous)
- Safe for concurrent operations on different repositories

### Error Handling

Errors are propagated to the LLM via `git_operation_error` events:

- Authentication failures
- Network errors
- Repository not found
- Invalid paths
- Merge conflicts (future)

## Limitations

1. **No SSH Key Support**: Implementation only supports username/password (HTTPS)
2. **Limited Merge Support**: Pull operation only handles fast-forward merges automatically
3. **No Commit Creation**: Cannot stage files and create commits
4. **No Submodule Support**: Cannot clone or update submodules
5. **No Conflict Resolution**: LLM cannot resolve merge conflicts (manual resolution required)
6. **No Rebase Support**: Rebase operations are not implemented
7. **No Tag Deletion**: Can create and list tags, but cannot delete them

## Future Enhancements

1. **SSH Authentication**: Add support for SSH keys
2. **Full Merge Support**: Implement automatic merge for non-fast-forward cases
3. **Commit Creation**: Allow LLM to stage files and create commits
4. **Tag Deletion**: Add ability to delete tags (local and remote)
5. **Branch Renaming**: Add ability to rename branches
6. **Advanced Operations**: Cherry-pick, rebase, stash
7. **Submodule Support**: Clone and update Git submodules
8. **Conflict Resolution**: Interactive conflict resolution via LLM

## Testing Strategy

See `tests/client/git/CLAUDE.md` for test implementation details.

`tests/client/git/operation_events_test.rs` pins the loop above: a temp repository
with one commit, an in-process mock model, `git_connected` → `git_log` →
`git_operation_completed` **matched on the commit subject appearing in `output`** →
`disconnect`. Matching on the payload rather than the event id is what makes it a
test of the report rather than of a name. A second case does the same for
`git_operation_error` via a checkout of a branch that does not exist.

Both `e2e_test.rs` cases remain `#[ignore]`d: they clone from github.com, which
CLAUDE.md forbids. Note one of them mocks `git_operation_completed` — written
against an event that could not fire, and never noticed because the test never ran.

**Test Approach:**

- Use local Git repositories for testing
- Test clone from public GitHub repositories (no auth required)
- Test authenticated operations with test credentials
- Verify LLM receives correct events
- Validate operation results

**LLM Call Budget:** < 10 LLM calls per test suite

## Security Considerations

### Filesystem confinement — implemented, not "future"

Every path this client touches comes from the model. `sandbox.rs` bounds them to one root
directory. **What is and is not confined is the part to read carefully.**

**Confined.** Every model-supplied path is resolved against the `allowed_root` startup
parameter and **refused** if it lands outside. Three entry points, and all twelve verbs:

| Entry point | Checked where | Note |
|---|---|---|
| `local_path` startup parameter | `connect_with_llm_actions` | Refused at startup, because it is also the default clone destination |
| `remote_addr` | `connect_with_llm_actions` | Refused at startup **when it names a real directory** outside the root — it may legitimately be a clone URL, and a non-existent value simply seeds no repository, as before |
| the repository `Repository::open` *landed on* | same | `open` walks **upward** looking for a `.git`, so the result is re-checked: the path handed in can be inside the root while the repository it found is above it |
| `git_clone`'s `path` | `run_git_operation` | The clone destination |
| `git_clone`'s `url`, when local | `run_git_operation` | A `file://` or bare-path source. Reading someone else's repository into the workspace is an exfiltration step, not a harmless read — once it is there, a permitted `git_push` can publish it. `file://` contains `://`, so "has a scheme means remote" is a hole; `classify_clone_source` unwraps it |
| the open repository, for every other verb | `require_repo()` | `git_fetch`, `git_status`, `git_list_branches`, `git_log`, `git_pull`, `git_push`, `git_checkout`, `git_delete_branch`, `git_list_tags`, `git_create_tag`, `git_diff` |

`require_repo()` re-checks rather than trusting the two places that can set `repo_path`. It
costs one `canonicalize` per operation and makes the property **local to the function**: each
verb is confined because it checks, not because of an argument about what could have reached
a session field. It also catches what the entry-point checks structurally cannot — a
workspace repository moved, or replaced by a symlink, after it was opened.

**How escape is prevented.** Canonicalisation, not string comparison: `<root>/a/../../etc`
and `<root>/link -> /etc` are both textually inside the root. `resolve` canonicalises the
**longest existing ancestor** — where every symlink and every resolvable `..` lives — then
re-appends the non-existent tail, refusing any `..` in that tail because nothing real remains
for it to mean. A clone destination that does not exist yet is therefore still checkable.

**Refuse, never relocate.** A path outside the root is an error naming `allowed_root`, not a
path quietly rewritten to sit inside. A model that asked for `/tmp/x` and got `<root>/tmp/x`
has a confusing bug where a refusal is a clear one, and the silent rewrite would also make
`git_clone` report success for a repository that is not where the model believes it is.

A *relative* path is different and **is** resolved against the root rather than the process's
cwd. That is not a relocation — a relative path names no location until something supplies a
base — and resolving against the cwd would mean `./my-repo`, which every startup example
uses, was refused for being outside the root.

**Default root**: the platform local-data directory —
`~/Library/Application Support/netget/git-workspace` on macOS,
`~/.local/share/netget/git-workspace` on Linux. Deliberately neither `$HOME` nor the cwd (for
a developer the cwd is this repository, which is the thing being protected). **Not**
`~/.netget/git-workspace`: `Settings::settings_path` makes `~/.netget` NetGet's settings
*file*, so `create_dir_all` beneath it fails with `ENOTDIR`.

**Destructive verbs: only the ones confinement cannot reach are gated.** `allow_remote_writes`
(default **false**) gates `git_push` and `git_delete_branch` *when a `remote` is given*.
Confinement bounds what happens on this disk and says nothing about where a push **goes** —
the remote URL comes out of the cloned repository's own config and the push carries this
client's credentials, so it can publish to or delete a branch on a real forge, and deleting
the workspace does not undo it.

`git_checkout` and a `force` **local** branch delete are deliberately **not** gated. Their
damage is confined to `allowed_root`, which is a scratch workspace; gating them would train
the operator to leave the flag on for routine work, and an opt-in that is always on means
nothing.

### Not confined, and worth knowing

- **Network clone/fetch/pull reach arbitrary URLs.** `git_clone https://…` contacts whatever
  host the model names. That is what a Git client is for; confinement bounds where the result
  lands, not who is contacted.
- **Credentials live in memory** for the session (`username` / `password` startup
  parameters) and are handed to `Cred::userpass_plaintext`. Prefer a PAT over a password, and
  scope it.
- **A TOCTOU window remains**: a symlink created between the check and libgit2's own `open()`
  is not seen. Closing it needs `openat2(RESOLVE_BENEATH)` or a per-operation chroot, neither
  of which libgit2 exposes. Exploiting it requires an attacker who can already write inside
  the workspace — a strictly larger capability than anything this guard defends against.
- **The workspace is shared between clients** and persists across runs. One Git client can
  read and modify what another cloned. Give each client its own `allowed_root` if that
  matters.

`tests/client/git/sandbox_test.rs` pins all of the above — both the boundary itself and the
boundary *as wired*, through a real client and `AppState::send_to_client`, because a guard
that is correct but unreachable is the failure mode worth testing for.

### Other considerations

1. **Clone Arbitrary URLs**: the LLM could clone a malicious repository. The contents land
   inside `allowed_root`; NetGet does not execute anything from them, but a hook in a cloned
   repository is not run by libgit2 either, so the risk is what the operator does with the
   workspace afterwards.
2. **Run NetGet with minimal file system permissions** — confinement is defence in depth, not
   a substitute for the OS boundary.

## Example Usage

**User Prompt:**

```
"Clone the repository https://github.com/rust-lang/rust.git to ./rust-repo and show me the last 5 commits"
```

**LLM Flow:**

1. Receives `git_connected` event
2. Issues `git_clone` action with URL and path
3. Receives `git_operation_completed` event (clone success)
4. Issues `git_log` action with `max_count: 5`
5. Receives `git_operation_completed` event with commit history
6. Summarizes results to user

## Dependencies

```toml
[dependencies]
git2 = { version = "0.18", optional = true }
```

**System Requirements:**

- libgit2 (installed via package manager: `apt-get install libgit2-dev` or `brew install libgit2`)

## References

- [git2-rs Documentation](https://docs.rs/git2/)
- [libgit2 Documentation](https://libgit2.org/docs/)
- [Git Internals](https://git-scm.com/book/en/v2/Git-Internals-Plumbing-and-Porcelain)

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running Git client. The handle is
registered **before** the `git_connected` LLM call, because a dashboard-created client
defaults to a `*` → manual rule and that call can park for minutes waiting for a human.

This is the archetype-(a) adoption. The session — `repo_path`, `username`, `password` — used
to be three `&mut Option<...>` locals owned by the connect task, which is exactly why nothing
outside that task could run a Git action: the working repository was unreachable. They are now
a `GitSession` behind an `Arc<Mutex<_>>` shared by the LLM path and the command loop, so
`[ send ]` operates on the repository the LLM just cloned. Both paths call one
`execute_git_action`.

**Outcome semantics — `Executed`, never `Sent`.** git2 owns whatever sockets an operation uses
(clone/fetch/pull/push do real network I/O) and reports no byte counts, so a number here would
be invented. The detail carries the real result — `git_status: dashboard-marker.txt; …`,
`git_push: <refspec result>`, `git_log: 12 line(s)`. An action the protocol refuses is
`Rejected`; a git2 operation that ran and failed is an `Err` (see below); `disconnect` is
`Disconnected`.

Three defects this surfaced, all fixed:

- **Every verb but `git_clone` was a silent no-op when no repository was open.** The arms were
  `if let Some(ref path) = repo_path { … }` with no `else`, so the operation simply did not
  happen and nothing said so. `run_git_operation` now returns a named error.
- **`remote_addr` pointing at an existing local repository did nothing.** `repo_path` started
  `None` and only a clone could set it, although this file has always documented `remote_addr`
  as "a repository URL (for cloning) **or** a local path (for existing repo)". Connect now
  tries `Repository::open(&remote_addr)` and seeds the session from it.
- **git2 was called straight from async context.** Every operation blocked a runtime worker.
  The whole dispatch now runs on `spawn_blocking`.

`Rejected` vs `Err` is distinguished by a `RejectedAction` marker error: the protocol refusing
an action (unknown type, missing field) is `Rejected`, a git2 operation that ran and failed is
an `Err` the operator sees as a failure. Neither is reported as success.

### The "bypasses the rate limiter" note in the root CLAUDE.md is stale for this client

That note says `git` and `mercurial` "call `generate_with_retry` directly, bypassing the rate
limiter, the retry/repair loop, and event-handler dispatch". Nothing under `src/client/git/`
or `src/server/git/` calls `generate_with_retry` any more: this client uses
`call_llm_for_client` (so client `event_handlers` are dispatched, budget is debited, and the
limiter applies) and `src/server/git/mod.rs` uses `action_helper::call_llm`. The only callers
of `generate_with_retry` left in the tree are inside `src/llm/` itself.
