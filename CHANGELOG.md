# Changelog

## 0.2.0 (unreleased)

Security and spec fixes for the server, store, client and federation auth. Several are breaking; see below.

### Breaking changes

- `TaskStore` changed. It has a new `is_visible(task_id, owner)` method; the default goes through `get_record`, and stores can override it with a lookup that does not copy the task. `insert` takes the owner (`insert(task, owner: Option<String>)`) and must fail with `A2aStoreError::TaskAlreadyExists` instead of replacing a task. `list` takes the caller (`list(req, owner: Option<&str>)`) and returns only that owner's tasks. New required method `get_record`, which returns the task with its owner; `get` now has a default implementation on top of it. Custom stores must also refuse changes to terminal tasks (see below).
- The server ignores client-chosen ids for new tasks. A message whose `taskId` does not name an existing task the caller can see gets `TaskNotFoundError` (-32001) instead of creating a task with that id.
- Completed, failed, canceled and rejected tasks are final. `update_status`, `add_artifact` and `append_message` return `A2aProtocolError::TaskTerminal` for them, and the server answers messages and `SubscribeToTask` calls for them with `UnsupportedOperationError` (-32004). Handlers that update a task after completing it now get an error.
- New tasks always have a `contextId`: the message's, or a server-generated one if the message has none. A follow-up message whose `contextId` differs from its task's `contextId` is rejected with invalid params (-32602). Each caller has its own `contextId` namespace: two callers can use the same value without seeing each other's tasks.
- Streams from `SendStreamingMessage` and `SubscribeToTask` start with the `Task` (A2A 3.1.2 and 3.1.6). Subscribing to a task that is waiting for input or auth returns that `Task` and closes.
- `A2aServer::start()` binds to `127.0.0.1` instead of `0.0.0.0`. Use `with_bind_addr` to listen on other interfaces. It now runs its own accept loop: HTTP/1.1 only (no HTTP/2), at most 1,024 open connections (`with_max_connections`), and 30 s to send request headers, which also covers a silent new connection and an idle keep-alive one. Out-of-descriptor accept errors back off up to 1 s; other listener errors end the server with an error. `serve(listener)` and `serve_with_shutdown(listener, signal)` do the same on a listener you bound.
- The server answers 408 when a request body takes longer than 30 s, and 429 (code -31429) when a caller already has 100 streams open. Both are configurable.
- `InMemoryTaskStore::new()` is bounded (`StoreLimits`): 10,000 tasks and 1 GiB of history and artifacts in total, 1,000 tasks and 256 MiB per owner, and 1,000 messages or 16 MiB of history per task. Terminal tasks are evicted after an hour without updates, submitted and working tasks after 24 hours (`active_ttl`), and input-required/auth-required tasks are kept (`interrupted_ttl` is off). When an owner hits its own limit, the store evicts that owner's least recently updated terminal task. When the whole store is full, it evicts a terminal task of an owner holding more than its fair share, then one of the inserting owner's own. Active tasks are never evicted to make room; if nothing can be evicted the insert fails (`OwnerCapacityExceeded`, `CapacityExceeded`, `OwnerStorageFull`, `StorageFull`). A message past the history limit gets `HistoryFull`, which the server reports as invalid params (-32602). Without an authenticator every caller is the same owner, so the per-owner limits apply to the whole server. Use `InMemoryTaskStore::with_limits` to change this.
- `A2aClient` has timeouts: 10 s connect, 60 s per unary request (and for the response headers of a stream), 5 min idle on a stream. A timeout is reported as `A2aTransportError::Timeout`.
- `A2aClient::send_streaming` and `subscribe` return the server's JSON-RPC error (for example `UnsupportedOperation` for a finished task, `TaskNotFound` for an unknown one) instead of an empty stream. A stream that ends in the middle of an event yields an error. Unary response bodies over 16 MiB are refused.
- `FederationPolicy::require_auth` defaults to `true`, both in `Default` and when the field is missing from a config file.
- `FederationPolicy::method_scopes` is an allowlist once it has any entry: a method without an entry, or with a misspelled key, is refused. An empty map still means no scope rules. List every method callers need, including `SendStreamingMessage`, `ListTasks` and `SubscribeToTask`.
- With `require_auth: false`, a token that is sent is still validated (invalid gives 401) and binds the caller's tasks, in `federation_auth_layer` as well as through `Authenticator`. Callers without a token share the `anonymous` principal in both paths.
- `federation_auth_layer` now checks `method_scopes`, only lets `GET`/`HEAD` requests for the exact Agent Card paths through without a token (it used to exempt any path containing `.well-known/agent-card.json`), rejects `POST` bodies without a JSON-RPC method, and answers auth failures with code -31401 instead of -32003 (which is `PushNotificationNotSupportedError`).
- `build_mtls_client` fails if `ca_cert_path` is set but cannot be read, instead of falling back to the system roots.
- New enum variants: `A2aProtocolError::TaskTerminal` and `A2aError::Store` (with `A2aStoreError`). The enums are `#[non_exhaustive]`.
- The `server` feature uses `futures` and `hyper-util` instead of `tokio-stream`; `federation` adds `subtle`.

### Added

- `auth` module with the `Authenticator` trait and `Principal`. `A2aServer::with_authenticator` authenticates every JSON-RPC request before reading its body (HTTP 401 on failure), calls `Authenticator::authorize` with the v1.0 method name (HTTP 403 on refusal), binds each new task to the caller, and scopes `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask` and follow-up messages to the caller's own tasks. Another caller's task is reported as not found.
- `FederationPolicy` implements `Authenticator`, with `method_scopes` checked per call. v0.3 aliases (`message/send`, ...) are held to the scopes of their v1.0 names.
- The server also accepts a `Principal` placed in the request extensions by an outer layer, such as `federation_auth_layer`.
- `server::canonical_method`, `TaskState::is_terminal`, `TaskState::is_interrupted`, `StoreLimits`, `TaskRecord`, `A2aClient::from_http_client`, `with_request_timeout`, `with_stream_idle_timeout`, `A2aServer::with_bind_addr`, `serve`, `with_header_read_timeout`, `with_body_read_timeout`, `with_max_streams_per_caller`, `with_max_connections`, `serve_with_shutdown`, `TaskStore::is_visible`, `FederationPolicy::principal_for`.
- A startup warning when the server listens on a non-loopback address without an authenticator.

### Fixed

- `SendStreamingMessage` with an existing `taskId` replaced that task (store insert was a plain map overwrite). It now appends the message to the task.
- A follow-up message with only a `taskId` now gets the task's `contextId`.
- `ListTasks` returned every caller's tasks.
- SSE streams from `SendStreamingMessage` and `SubscribeToTask` never closed. They now end after the event that moves the task to a terminal state or to input-required/auth-required, and when the task is evicted or expires. The server sends SSE keep-alives.
- `CancelTask` on a rejected task succeeded.
- A task canceled before its handler started still ran the handler.
- A handler that panicked left its task in Working forever. The task is now marked Failed.
- Federation tokens are compared in constant time (`subtle`), an empty Bearer token never matches, and the `Bearer` scheme is matched case-insensitively.
- The server re-checks ownership of the tasks a `TaskStore` returns from `list`, so a store that filters wrongly cannot leak other callers' tasks. When it has to drop any, it also resets `totalSize` and `nextPageToken`.
- The client's SSE parser follows the SSE line format: LF, CRLF or CR line endings, `data:` with or without a space, multi-line data, comments. It also reads A2A v1.0 events wrapped in JSON-RPC responses. It no longer corrupts multi-byte UTF-8 split across chunks, scans each byte once, and ends a stream with an error once a single event passes 16 MiB.

## 0.1.2 and earlier

See the git history.
