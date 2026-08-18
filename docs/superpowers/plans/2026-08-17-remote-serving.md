# Remote serving — Implementation plan

**Date:** 2026-08-17
**Phase:** 3 of the semantic-search work
**Status:** approved for implementation

## What this adds

mem8 in a container, reachable over the network, so a team shares one memory
rather than each developer keeping a private SQLite file.

```
[agent] --stdio--> [mem8, local]                      ← today, unchanged
[agent] --https--> [mem8 container] --> [pgvector]    ← this plan
```

## Why this is not just a config file

stdio is a private channel between one agent and one child process it spawned.
Nothing about it needs auth: the OS already decided who may run the binary, and
the memory is one user's own.

HTTP is the opposite on every count. The transport is shared, the caller is
unauthenticated, and the server's own filesystem tells it nothing about who is
asking. Three assumptions that hold silently over stdio all break at once:

| Assumption | Over stdio | Over HTTP |
|---|---|---|
| The caller is the owner | The OS enforced it | Anyone who can reach the port |
| `cwd` identifies the project | The agent's working directory | `/app`, for every client alike |
| The channel is private | A pipe between two processes | A network, possibly the internet |

The second is the subtle one, and the reason this is not just a transport swap.
`detect_scope` walks up from the process's working directory looking for `.git`.
In a container that is `/app` for every request, so **every project silently
collapses into one scope** — memories mixed together, and no error to notice.

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Transport | rmcp `StreamableHttpService` | Verified present in rmcp 0.9.1, our pinned version. It is a `tower::Service`, so it composes with axum and auth is ordinary middleware. |
| Scope in HTTP mode | Required explicitly | The server cannot infer it. Refusing is the only honest answer; a default would mean silently writing into the wrong project. |
| Auth | Static bearer token, `MEM8_TOKEN` | Right size for self-hosting. Compared in constant time. |
| TLS | Required for non-loopback binds | A bearer token over plaintext HTTP is not a secret. Refusing to start is what makes this hard to get wrong by accident. |
| stdio mode | Entirely unchanged | It is the default and the common case. Nothing here may alter it. |

## Scope: fail loudly

`Memory8::resolve_scope` falls back to `detect_scope(cwd)` when no project is
given. That is right locally and wrong remotely, so the service learns which
mode it is in:

```rust
pub enum ScopeMode {
    /// Detect from the working directory. stdio, and every CLI subcommand.
    Detect,
    /// Require the caller to name it. HTTP.
    Explicit,
}
```

In `Explicit` mode a call with no project is `InvalidInput`, naming the field
and saying why. **No default, no `"default"` project, no cwd fallback.** Writing
a memory into the wrong project is worse than refusing the write: the user is
told about a refusal, and never told about a silent misfile.

`global: true` on search stays allowed — it is explicit about crossing
projects, which is the opposite of the failure being prevented.

## Auth

Bearer token in `Authorization`, checked by axum middleware before the request
reaches the MCP service.

- **Constant-time comparison.** A byte-by-byte `==` leaks the token's prefix
  through timing. `subtle::ConstantTimeEq`.
- **No token configured → refuse to start in HTTP mode.** Not "allow all". A
  server that starts unauthenticated because a variable was unset is how
  memories end up public.
- **Minimum length.** A one-character token is not a secret; refuse it at
  startup rather than pretending.
- **401 with `WWW-Authenticate`,** and no detail about why. "Bad token" and
  "no token" are the same response.
- **Never logged.** Not on startup, not in errors.

## TLS

Non-loopback bind without TLS is a startup error, not a warning:

```
mem8 serve --http 0.0.0.0:8080
  error: refusing to bind 0.0.0.0:8080 without TLS.
  A bearer token sent over plaintext HTTP is readable by anything on the path.
  Either pass --tls-cert and --tls-key, or bind 127.0.0.1 and terminate TLS
  in a reverse proxy.
```

Loopback without TLS stays allowed — that is the reverse-proxy shape, where the
proxy holds the certificate and mem8 never sees plaintext leave the host.

`--insecure` exists as an explicit override for a trusted private network. It
must be typed deliberately and is refused in combination with a public bind
unless the operator also passes it, which is the point: the failure mode is a
person choosing, not a default.

## The container

mem8 needs `MEM8_DB` pointing at the database service, and the image should not
carry a compiler. Multi-stage: build with the Rust toolchain, copy the binary
into a slim runtime, run as a non-root user.

The compose file gains an `app` service alongside `db`, but the default
`docker compose up` must still be the database-only shape that phase 1
established — remote serving is opt-in via a profile, so a developer running
`docker compose up` for local Postgres does not accidentally publish a server.

## Testing

| Test | Proves |
|---|---|
| No token configured → server refuses to start | Not fail-open |
| Wrong token → 401 | The check runs |
| No `Authorization` header → 401 | Missing is not absent-therefore-allowed |
| Correct token → the MCP handshake completes | Auth does not break the protocol |
| Token comparison is constant-time | The mitigation is real, not decorative |
| Non-loopback + no TLS → refuses to start | The TLS guard holds |
| Loopback + no TLS → starts | The reverse-proxy shape still works |
| `Explicit` mode, no project → `InvalidInput` naming the field | The scope hole is closed |
| `Explicit` mode, project given → normal write | Not closed too far |
| `Detect` mode unchanged | stdio did not regress |
| Full handshake over real HTTP, add then search | The transport works end to end |

The last is the one that matters most and the one most easily faked; it runs
against a real bound socket, not a mocked service.

## Order

1. `ScopeMode` in `core`, with tests. No HTTP yet — provable on its own.
2. Auth middleware, with tests, still no server.
3. `mem8 serve --http`, wiring the two together, with the TLS guard.
4. An end-to-end test over a real socket.
5. Dockerfile and the compose profile.
6. Docs, including an explicit statement of what this exposes.

Each step leaves stdio untouched and the tree green.

## What this does not do

- **Multi-tenancy.** One token, one trust level. Every caller who can
  authenticate sees every project. Per-token project isolation was considered
  and deferred; it needs a token store, and this plan is for self-hosting rather
  than a shared service.
- **Rate limiting or audit logging.** Neither exists. A stolen token is
  unlimited and leaves no trace beyond the memories it changes.
- **Token rotation.** Restart the server with a new value.

These are stated so the deployment decision is made knowingly. A mem8 exposed to
the public internet is not what this builds; a mem8 on a private network or
behind a VPN, with TLS and a real secret, is.
