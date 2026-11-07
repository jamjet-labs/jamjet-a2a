# Changelog

## 0.2.0 (unreleased)

Security and spec fixes for the server, store, client and federation auth. Several are breaking; see below.

### Breaking changes

- `TaskStore` changed. `insert` takes the owner (`insert(task, owner: Option<String>)`) and must fail with `A2aStoreError::TaskAlreadyExists` instead of replacing a task. `list` takes the caller (`list(req, owner: Option<&str>)`) and returns only that owner's tasks. New required method `get_record`, which returns the task with its owner; `get` now has a default implementation on top of it. Custom stores must also refuse changes to terminal tasks (see below).
- The server ignores client-chosen ids for new tasks. A message whose `taskId` does not name an existing task the caller can see gets `TaskNotFoundError` (-32001) instead of creating a task with that id.
- Completed, failed, canceled and rejected tasks are final. `update_status`, `add_artifact` and `append_message` return `A2aProtocolError::TaskTerminal` for them, and the server answers messages and `SubscribeToTask` calls for them with `UnsupportedOperationError` (-32004). Handlers that update a task after completing it now get an error.
- New tasks always have a `contextId`: the message's, or a server-generated one if the message has none. A follow-up message whose `contextId` differs from its task's `contextId` is rejected with invalid params (-32602).
- `A2aServer::start()` binds to `127.0.0.1` instead of `0.0.0.0`. Use `with_bind_addr` to listen on other interfaces.
- `InMemoryTaskStore::new()` is bounded: at most 10,000 tasks, and tasks idle for an hour are evicted. When full, it evicts the least recently updated terminal task, or refuses the new task with `A2aStoreError::CapacityExceeded` if every task is still active. Use `InMemoryTaskStore::with_limits` to change this.
- `A2aClient` has timeouts: 10 s connect, 60 s per unary request (and for the response headers of a stream), 5 min idle on a stream. A timeout is reported as `A2aTransportError::Timeout`.
- `FederationPolicy::require_auth` defaults to `true`, both in `Default` and when the field is missing from a config file.
- `federation_auth_layer` now checks `method_scopes`, only lets `GET`/`HEAD` requests for the exact Agent Card paths through without a token (it used to exempt any path containing `.well-known/agent-card.json`), rejects `POST` bodies without a JSON-RPC method, and answers auth failures with code -31401 instead of -32003 (which is `PushNotificationNotSupportedError`).
- `build_mtls_client` fails if `ca_cert_path` is set but cannot be read, instead of falling back to the system roots.
- New enum variants: `A2aProtocolError::TaskTerminal` and `A2aError::Store`. Both enums are `#[non_exhaustive]`.
- The `server` feature uses `futures` instead of `tokio-stream`.

### Added

- `auth` module with the `Authenticator` trait and `Principal`. `A2aServer::with_authenticator` authenticates every JSON-RPC request before parsing it (HTTP 401 on failure), calls `Authenticator::authorize` with the v1.0 method name (HTTP 403 on refusal), binds each new task to the caller, and scopes `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask` and follow-up messages to the caller's own tasks. Another caller's task is reported as not found.
- `FederationPolicy` implements `Authenticator`, with `method_scopes` checked per call. v0.3 aliases (`message/send`, ...) are held to the scopes of their v1.0 names.
- The server also accepts a `Principal` placed in the request extensions by an outer layer, such as `federation_auth_layer`.
- `server::canonical_method`, `TaskState::is_terminal`, `TaskState::is_interrupted`, `StoreLimits`, `TaskRecord`, `A2aClient::from_http_client`, `with_request_timeout`, `with_stream_idle_timeout`, `A2aServer::with_bind_addr`.
- A startup warning when the server listens on a non-loopback address without an authenticator.

### Fixed

- `SendStreamingMessage` with an existing `taskId` replaced that task (store insert was a plain map overwrite). It now appends the message to the task.
- `ListTasks` returned every caller's tasks.
- SSE streams from `SendStreamingMessage` and `SubscribeToTask` never closed. They now end after the event that moves the task to a terminal state, and when the task is evicted. The server sends SSE keep-alives.
- `CancelTask` on a rejected task succeeded.
- A task canceled before its handler started still ran the handler.
- Federation tokens are compared in constant time, and an empty Bearer token never matches.
- The client's SSE parser no longer corrupts multi-byte UTF-8 split across chunks.

## 0.1.2 and earlier

See the git history.
