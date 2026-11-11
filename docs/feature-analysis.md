# Native Rust port: dependencies behind the four visible features

## Executive summary

Read-only source analysis of upstream **7e565c510d6dfaef2e1fd97bf9d89a97ce9cece7**, checked locally, dated 2026-11-11. Paths below are relative to `.ref/t3code/`. No upstream scripts, installs, provider processes, or servers were run. Suggested Rust approaches are design proposals, not verified implementations or dependency-version recommendations.

The visible scope is small, but its essential backend is not: projects and threads need durable identities and ordering; chat needs a run/session state machine, persistent messages and activities, cancellation and approval continuations; Claude needs a bidirectional SDK bridge and Windows executable/environment handling; the git applet needs checkout-aware status and invalidation. A functioning message list alone does not cover these dependencies.

Three particularly consequential findings:

1. **Current upstream runs orchestration V2.** Do not port the older projection/turn model just because its migration files remain. V2 distinguishes app threads, runs/attempts, execution nodes, provider sessions/threads/turns, messages, runtime requests and timeline items. Its event adapter now writes a shared application event log.
2. **Claude is driven through `@anthropic-ai/claude-agent-sdk`, not a T3-authored `claude --output-format stream-json` launcher.** The adapter calls `query()` with an asynchronous prompt stream and SDK options, supplies its own process spawn callback and consumes SDK messages. Replacing this with a raw Rust CLI client is a protocol implementation project.
3. **The branch UI has several cooperating pieces.** Branch/workspace selection, working-tree changes, divergence and PR status are not one `git branch` invocation. Local status, cached remote status, explicit refreshes and subscription-owned background polling have separate paths.

For the shortest PoC, keep a narrow Node sidecar for the existing Claude SDK and implement the local app domain, SQLite storage and git subprocess service in Rust. Keeping the entire upstream server is another viable compatibility shortcut, but requires implementing Effect RPC framing and its subscription/snapshot model. A fully in-process Rust application still needs a provider subprocess; the JavaScript SDK cannot simply be linked as a Rust crate.

Priority interpretation: **MUST** supports the four stated visible features; **SHOULD** closes important usability or fidelity gaps; **LATER** is outside that minimal demonstration. Advanced features can be omitted explicitly, but must not leave misleading controls or broken pending states.

## Architecture overview

```text
UPSTREAM
 Electron main/preload                  React web client
 desktop backend pool ---- bootstrap --> client-runtime state/atoms
           |                                     |
           | Node server process                  | Effect RPC JSON / WebSocket /ws
           v                                     v
       server.ts / ws.ts: auth, handlers, subscriptions
           |
           +--> project commands / orchestrator V2 / command policy
           |          |
           |          +--> transaction: events + projections + receipts + outbox
           |          |                         SQLite statev2.sqlite
           |          +--> effect execution / provider session manager
           |                            |
           |                      ClaudeAdapterV2
           |                            |
           |                      Agent SDK query()
           |                            | stdin/stdout pipes, control callbacks
           |                        Claude Code process
           |
           +--> GitWorkflowService -> GitManager / VCS driver -> git process
           +--> workspace search / attachments / terminal PTY / settings

SUGGESTED LOCAL PoC
 GPUI views <--- typed snapshots/events --- Rust application service
    |                                      | SQLite + git subprocess workers
    +--- typed commands -------------------+
                                           |
                                  small Node SDK sidecar
                                           |
                                       Claude Code
```

## 1. Client/server boundary and bootstrap

**Files:** `apps/server/src/server.ts`, `apps/server/src/ws.ts`; `packages/contracts/src/rpc.ts`, `packages/contracts/src/orchestrationV2.ts`, `packages/contracts/src/orchestrationDispatch.ts`, `packages/contracts/src/git.ts`; `packages/client-runtime/src/rpc/{protocol,session,client}.ts`; `apps/web/src/bootstrap.ts`; `apps/desktop/src/backend/{DesktopBackendManager,DesktopBackendPool,DesktopBackendConfiguration,DesktopLocalEnvironmentAuth}.ts`; `apps/desktop/src/preload.ts`; `apps/server/src/bootstrap.ts`.

The web UI does not invoke provider processes directly. `WsRpcGroup` declares typed RPC endpoints with Effect schemas for payload, success and error values. `ws.ts` serves `/ws` with Effect's WebSocket RPC server and `RpcSerialization.layerJson`, performs authorization and supplies handlers. This is **Effect RPC over JSON**, not evidence of standard JSON-RPC 2.0 compatibility. Unary requests coexist with streaming subscriptions, cancellation and stream flow control. `terminal/OutputProtocol.ts` adds an output window; orchestration `LiveStreamBudget.ts` bounds replay/live delivery.

`client-runtime/rpc/session.ts` builds the socket/RPC session; `client.ts` routes requests by environment and distinguishes subscriptions from commands. `protocol.ts` specifies a 15-second no-frame ping timeout, with a comment describing five-second pings. Reconnection and error presentation are meaningful state, not simply a spinner. The desktop is an Electron host of the web surface plus backend processes, local authentication/bootstrap and native IPC. Server bootstrap can read a supplied bootstrap envelope/file descriptor; it is not necessary to recreate Electron IPC in GPUI.

The orchestration API exposes dispatch, shell subscription and per-thread subscription separately. Here **shell means the lightweight application/sidebar read model**, not a command shell. A native port should not confuse `subscribeShell` with terminal output.

### Rust port notes

**MUST:** define a typed command/event boundary between GPUI and background services. Use `serde` DTOs, bounded channels and an async worker runtime such as Tokio; keep blocking database/process work off the UI executor. The single-machine version can replace WebSockets with in-process calls and notifications.

If retaining upstream Node, use `tokio-tungstenite` or equivalent and deliberately implement the Effect wire protocol, auth, reconnect, ACK/cancel semantics and snapshot resynchronization. Merely sending method-name JSON will not reproduce the client. Remote environments, relays, mobile access and multi-device authorization are **LATER**.

## 2. Orchestration, entities, events and read models

**Files:** `apps/server/src/orchestration-v2/{Orchestrator,CommandPolicy,ThreadCommandExecutor,EventSink,EventStore,ProjectionStore,ProjectionMaintenance,CommandReceiptStore,EffectOutbox,RunExecutionService,RunFinalizationService,ProviderSessionManager,QueuedRunOrder,WireProjection,TurnItemPositionStore}.ts`; `apps/server/src/persistence/OrchestrationEventStore.ts`; `packages/contracts/src/orchestrationV2.ts`; `packages/client-runtime/src/state/{orchestration,orchestrationV2Projection}.ts`; `packages/shared/src/orchestrationV2BoundedSnapshot.ts`.

The traced command path is client command → RPC dispatch → `Orchestrator`/command policy → serialized thread execution → `EventSink` transaction → projections and committed publications. Concrete contracts include `thread.create`, `message.dispatch`, `run.interrupt`, `runtime-request.respond`, `queued-run.reorder`, `queued-run.cancel` and `queued-run.edit`. Dispatch intents distinguish `steer_active`, `restart_active`, `queue_after_active` and `start_immediately`. Provider work is executed through services and a durable effect outbox, rather than while rendering UI or merely inserting a message. Command receipts provide command-ID retry/deduplication. Outbox records include status, attempts, availability and leases. Conditional event writes protect ownership of the active run attempt/provider thread so late output does not complete a newer turn.

The essential entity distinctions are:

| Entity | Purpose |
|---|---|
| Project | Directory-rooted workspace record and defaults. |
| App thread | Durable conversation, project reference and workspace/branch metadata. |
| Run / run attempt | Requested work and a particular execution attempt; includes queued, running and terminal lifecycle. |
| Execution node | Root/child work identity and parent relationships. |
| Provider session / provider thread | Runtime adapter/session ownership versus provider-native conversation identity. |
| Provider turn | Provider execution cycle belonging to an attempt; distinct from the app's run. |
| Conversation message | User/assistant content, attachments and attribution. |
| Turn item | Ordered timeline/work-log object: tools, reasoning, notices, plans and other activity. |
| Runtime request | Approval or user input requiring a response, with its own pending/resolved lifecycle. |
| Checkpoint / scope | Filesystem baseline/result identity for changes and rollback. |

Activity in the current path is represented by nodes and typed turn items; the old activity-projection model should not be assumed current. Provider-normalized updates include `node.updated`, `message.updated`, `turn_item.updated` and provider lifecycle updates. The persisted domain contract uses hyphenated `turn-item.updated`, `provider-thread.updated` and `runtime-request.updated`: adapter event names and domain event names are separate layers. Stable IDs and ordinals let the UI update an existing item rather than append duplicate rows.

`EventSink` batches ordered provider updates, persists them transactionally and publishes after commit. `ProjectionStore` maintains queryable records and lightweight thread shells; per-thread reads use bounded recent timeline windows and history pages instead of loading every conversation into the sidebar. `WireProjection` shapes events for transport. Client projections apply updates locally.

Snapshots carry sequence positions, and subscriptions reconcile replay with buffered live events. **This is not an indefinitely retained replay log:** `ProjectionMaintenance` retains a one-day replay/retry tail, prunes events already covered by projections and preserves required anchors/heads. Old reconnect cursors must resynchronize from a snapshot. Projection metadata records schema version and last sequence. `apps/server/src/orchestration-v2/ProviderRuntimeRecoveryService.ts` reconciles work whose old processes no longer exist; it is unsafe to restore a saved `running` flag as if a process survived.

### Rust port notes

**MUST:** model lifecycle and IDs explicitly, including queued/running/interrupted/failed/completed and pending approvals. Use a single application service or per-thread actor to serialize mutation, persist before publishing and reject stale output by attempt ID. SQLite plus typed Rust enums is sufficient; reproducing every upstream delegation node is not required.

Keep transactional message/activity updates and command deduplication. A reduced durable job table can replace the full outbox, provided crash recovery never blindly resubmits an already delivered prompt. Snapshot plus incremental events works in-process too. Cross-provider handoff, delegation graphs, scheduled tasks, guest collaboration and legacy replay/import are **LATER**.

## 3. Persistence, migrations and settings paths

**Files:** `apps/server/src/config.ts`; `apps/server/src/persistence/{Migrations,initializeV2Database,DatabaseStorage}.ts`; migrations `001_OrchestrationEvents.ts`, `002_OrchestrationCommandReceipts.ts`, `003_CheckpointDiffBlobs.ts`, `005_Projections.ts`, `049_ProjectionThreadsActiveOrderKey.ts`, `055_OrchestrationV2.ts`, `063_DropUnusedV1Projections.ts`, `076_ProjectionProjectsPinArchive.ts`; `Migrations/OrchestrationV2/{Foundation,ApplicationEventSource,ProviderSessionBindings}.ts`; `packages/shared/src/nodeSqliteClient.ts`; `apps/server/src/serverSettings.ts`.

`config.ts` derives `<baseDir>/userdata/statev2.sqlite`, `settings.json`, logs and attachments-related paths; dev URL mode can select `dev` rather than `userdata` when base-dir options do not override it. The normal T3 home is `~/.t3` (also documented in upstream `AGENTS.md`); home/base-directory overrides matter. `initializeV2Database.ts` handles initial V2 database creation/legacy copying. Never assume `state.sqlite` is the current database.

Migration 055 creates V2 projections for threads, runs, attempts, nodes, provider sessions/threads/turns, runtime requests, messages, plans, turn items, checkpoints and context transfers. Indexed columns support selection/ordering while JSON payloads preserve complete typed records. `Foundation.ts` adds driver/instance distinctions, effect outbox, item positions and projection metadata.

**Important schema correction:** 055 originally creates `orchestration_v2_events` and V2 receipts. `ApplicationEventSource.ts` migrates agent events into shared `orchestration_events`, marks application event version 2 and re-baselines project state. Current `EventStoreV2` delegates to `persistence/OrchestrationEventStore.ts`; current receipts use the shared receipt infrastructure. Thus those original V2 event-table names alone are not an accurate description of the active write path. Projects remain in `projection_projects`, accessed through `ProjectStore.ts`.

The SQLite adapter uses Node's `node:sqlite` and contains WAL-aware reader-worker handling. Schema migrations are registered centrally, not inferred from filesystem contents. `DatabaseStorage` performs maintenance/accounting, including incremental reclamation and event pruning scheduling.

`serverSettings.ts` reads, validates, patches and watches `settings.json`, including linked-file target changes; settings are not all columns in SQLite. Provider instance configuration and project overrides affect runtime policy. Credentials/secrets have separate handling in `settingsSecrets.ts` and `secrets/`; do not copy secret values into chat records or UI preference files. Other verified derived locations include `<stateDir>/keybindings.json`, `<stateDir>/attachments`, `<stateDir>/logs/server.trace.ndjson`, `<stateDir>/logs/provider/events.log`, `<baseDir>/worktrees` and `<baseDir>/caches/worktree-checkouts`.

### Rust port notes

**MUST:** own PoC data directory, schema version/migrations, transactional persistence and reliable restart behavior. `rusqlite` on a worker or `sqlx` are plausible choices. Prefer a separate Rust database over opening the user's live T3 database read-write. Source/schema compatibility is a separate migration feature, **LATER**. Use a small validated JSON settings file and atomic replacement; `directories` can supply a Windows-appropriate app directory. Log rotation and storage maintenance are **SHOULD**; full upstream compaction tooling is **LATER**.

## 4. Claude Code integration and Windows process behavior

**Files:** `apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts` (especially `ClaudeAgentSdkQueryRunner`, query option construction, runtime query policy, `canUseToolEffect`, SDK-message handling and `createClaudeAdapterV2`); `apps/server/src/provider/Drivers/{ClaudeExecutable,ClaudeHome,ClaudeSkills,ClaudeDriver}.ts`; `apps/server/src/claudeModelOptions.ts`; `apps/server/src/provider/{ClaudeModelCatalog,ClaudeTurnTokenUsage}.ts`; `apps/server/src/orchestration-v2/Adapters/{ProviderTextDeltaCoalescer,ClaudeTaskOutputTail}.ts`; `apps/server/package.json`.

### Launch and protocol ownership

The server declares `@anthropic-ai/claude-agent-sdk` `^0.3.321`. `ClaudeAdapterV2` imports SDK `query`, session/history APIs and SDK message/control types. Its query runner creates an asynchronous user-message prompt queue and calls `query({ prompt, options: { ...options, spawnClaudeCodeProcess } })`. The SDK query is held as a live object, with methods including interrupt, permission/model updates, MCP registration and asynchronous disposal. This is a long-lived controllable session, not a fresh interactive terminal for each message.

Verified launch inputs include `pathToClaudeCodeExecutable`, cwd/environment, `sessionId` for new conversations or `resume` for an existing native ID, optional `resumeSessionAt`, model, effort, tools, permissions, `includePartialMessages: true`, additional directories, settings and system-prompt additions. Configured launch arguments are parsed: `--add-dir`, `--permission-mode`, `--dangerously-skip-permissions`, `--append-system-prompt`, `--append-system-prompt-file` and `--settings` receive special handling. The adapter can request `--thinking-display summarized` through SDK extra arguments.

**Verification boundary:** the SDK implementation is not installed in this checkout. Exact SDK-generated argv, including input/output-format, verbose and control-protocol flags, cannot be verified from these source files. No claim that T3 itself builds a particular `--stream-json` command is justified. A Rust raw-CLI replacement must verify those flags and the control protocol separately; this report did not launch Claude to discover them.

### Session, resume, interruption and policy

`ProviderSessionManager` owns adapter sessions separately from durable provider-thread bindings. The Claude adapter reuses a compatible live query; settings changes can force replacement/resume. Changing thinking settings or clearing effort requires a new process, while some model/effort changes can update a live query. Native session identity is preserved for resume. The adapter also handles native history, compaction, prompt echoes, background tasks and results after root output. A provider result is not universally proof that every background task has finished.

Runtime mode defaults map as follows, subject to explicit sandbox/approval policies and provider availability:

| T3 mode | Claude SDK permission mode |
|---|---|
| approval-required | `default` |
| auto-accept-edits | `acceptEdits` |
| auto | `auto` |
| full-access | `bypassPermissions` plus dangerous-skip allowance |
| plan interaction | `plan`, with a permission callback installed |

Read-only policies can use `dontAsk` and restricted tools. Disabled bypass availability can downgrade to auto/default. `RuntimePolicy.ts` resolves workspace, project/provider defaults and managed ceilings; these modes are not interchangeable with an OS sandbox.

`canUseToolEffect` creates tool activity and runtime requests, suspends on answers and returns SDK allow/deny results. `AskUserQuestion`, plan exit and MCP elicitation have dedicated paths. Pending input must be identified and answered once; denying, cancelling and interrupted requests must unblock the callback. Interruption calls the SDK query's `interrupt()`; disposal and process exit are separate lifecycle steps.

### Output, tool data and usage

The adapter normalizes SDK messages into provider events, then orchestration persists and projects them for the timeline. Tool calls retain native IDs, name/input, lifecycle, parent relationships, formatted labels, output/error data, image outputs and edit/patch details. `toolActivity` and `toolOutput` helpers in `packages/shared/src/` support presentation; the work log should not be reconstructed by scraping assistant prose. `ClaudeTaskOutputTail` also tails background command output files.

**Streaming nuance:** root `stream_event` processing explicitly handles thinking blocks/deltas and message usage, using `ProviderTextDeltaCoalescer`; the inspected handler does not simply forward every ordinary `text_delta` as chat text. Ordinary assistant text is extracted from SDK assistant frames and emitted by `emitAssistantTextArtifacts` as node/message/turn-item updates, with native-ID deduplication. Preserve incremental provider events, but do not promise token-by-token ordinary text solely because `includePartialMessages` is enabled. SDK frame granularity at runtime remains unverified here.

`ClaudeTurnTokenUsage.ts` normalizes model-ledger usage, cached input and context windows. Message-start/message-delta usage, assistant snapshots and terminal result accounting must be reconciled, not added indiscriminately. The adapter also emits retry, rate-limit, compaction and failure information; a failed turn must not leave an endless working indicator.

### Windows specifics

`ClaudeExecutable.ts` explains that the SDK bypasses shell/PATH/PATHEXT lookup. On Windows it resolves the executable using shared `SpawnExecutableResolution`, then recognizes `.cmd`, `.bat` and `.ps1` shims. It searches beside a shim for `node_modules/@anthropic-ai/claude-code/bin/claude.exe`, then `.../cli.js`; unresolved shims produce a warning. The fallback JavaScript runtime behavior is SDK-owned and was not verified without SDK sources.

The Windows query callback directly calls Node `spawn(command, args, { cwd, env, signal, windowsHide: true, stdio: ['pipe','pipe','pipe'] })`. It supplies no shell or PTY. Therefore **ConPTY is not in the Claude transport path**, and there is no PowerShell/cmd prompt automation to port. Arguments are an array; shell quoting and a space-joined command string would change behavior. Shared `packages/shared/src/shell.ts` owns executable resolution, while user-provided launch argument parsing is in `cliArgs.ts`.

`ClaudeHome.ts` inherits host environment, merges instance overrides, sets `CLAUDE_CODE_ENTRYPOINT=t3-code`, removes inherited host-Claude session variables (including `CLAUDECODE`, session/messaging IDs and effort) and removes `ELECTRON_RUN_AS_NODE`. A configured Claude home becomes `CLAUDE_CONFIG_DIR`, rather than replacing HOME; otherwise it honors the inherited config directory/default `.claude`. Preserve PATH and credential discovery deliberately.

Unix spawning has a dedicated process-group wrapper and termination machinery. The inspected Windows branch uses a normal child and SDK disposal; startup-failure cleanup explicitly kills the owned child and waits for exit. **Recursive Windows descendant termination is not established by this path**: do not claim upstream's Unix group behavior also covers Windows or that a Job Object/`taskkill /T` is installed here. Shell choice used internally by Claude's own Bash/tool execution is also not determined by T3's SDK spawn callback.

### Rust port notes

**MUST:** prefer a small Node SDK sidecar with a deliberately narrow, versioned NDJSON protocol: start/resume, submit, interrupt, approval/input answer, close and normalized output events. Include request/run IDs, bounded output, exit/error events and EOF recovery. This preserves SDK control behavior without retaining the full server.

If implementing raw CLI control in Rust, use `tokio::process`, pipes, a framed JSON reader and an explicit permission/control request dispatcher; budget **L**, not S. Validate native executable and npm installations, spaces/non-ASCII paths, inherited PATH, fresh/resumed sessions, interrupted tools and shutdown. Use the `windows` crate and owned Job Objects if reliable descendant cleanup is required; that is a proposed improvement, not a verified upstream behavior. Basic model selection and approval-required mode are MUST; effort/plan controls and richer tool presentations are SHOULD unless shown in the PoC.

## 5. Git branch/status applet, worktrees and PRs

**Files:** `apps/web/src/components/{ChatView,GitActionsControl,BranchToolbar,BranchToolbarBranchSelector,BranchPicker}.tsx`; `apps/web/src/state/vcs.ts`; `packages/client-runtime/src/state/{vcs,vcsStatusRevalidation}.ts`; `apps/server/src/ws.ts`; `apps/server/src/vcs/{VcsStatusBroadcaster,GitVcsDriverCore,VcsProcess}.ts`; `apps/server/src/git/{GitWorkflowService,GitManager}.ts`; `apps/server/src/orchestration-v2/{WorktreeBranchSync,RunFinalizationService}.ts`.

The top/right git controls read repository status through `vcsEnvironment`; `GitActionsControl` uses `isRepo`, `refName`, `hasWorkingTreeChanges`, ahead/behind, remote availability and PR data to present actions and availability. Other controls select workspace/environment/worktree and branch; not every branch control is physically inside `ChatHeader.tsx`. `ChatView` composes these pieces. The applet can show changes totals, divergence and PR-related state/actions, not just a branch string.

The RPC handlers route `subscribeVcsBranch` and `subscribeVcsStatus` to `VcsStatusBroadcaster`, then `GitWorkflowService`/`GitManager` and the VCS driver. Verified git commands in `GitVcsDriverCore` include:

- Repository identity/root: `rev-parse --show-toplevel`, `rev-parse --git-common-dir`, full symbolic HEAD references. Detached/unborn/non-repository states require distinct results.
- Detailed status: `status --porcelain=2 --branch -z`, optionally `--no-ahead-behind`, plus index and diff/numstat reads for changes totals.
- Divergence: `rev-list --left-right --count HEAD...@{upstream}`, with separate default/base-branch comparison logic when appropriate.
- Branch lists/worktree occupancy: `for-each-ref` and `worktree list --porcelain`.
- Switch: guarded `checkout`, optional `--track`, and a trailing `--` so a stale ref cannot become an accidental path checkout. Option-like names are rejected. Creating a ref can optionally switch it.

`VcsStatusBroadcaster` caches/fingerprints local and remote results and publishes changes. A detailed subscription starts with a local/remote snapshot; remote pollers are retained by subscriptions, shared and released when no longer needed. Their interval is configurable, subject to background policy and exponential failure backoff (30 seconds to 15 minutes). Driver-level upstream refresh additionally coalesces fetches with a 15-second interval and five-second timeout; these are not proof of a fixed 15-second UI poll. Repository/ref resolution has separate short caches. Git subprocesses avoid optional index locks for concurrent status reads.

The web revalidation path refreshes status/branch when returning to the document, debounced 250 ms (`state/vcs.ts` and `lib/documentActivity.ts`); action completion invalidates/refreshes state, and `RunFinalizationService` refreshes workspace entries/local VCS status after work. **Do not describe the applet as a recursive filesystem watcher.** The traced status subscription is publication/cache plus remote polling and explicit refreshes. `GitVcsDriverCore` has a file watch for trace output, which is not a working-tree status watcher. No general recursive status watcher was verified.

Threads may use `worktreePath` rather than the project's root. Status/actions must target that checkout, while shared repository state is keyed with git common-directory awareness. Worktree branch synchronization reconciles thread metadata with actual branch state. Branch selection can reject occupancy conflicts; worktree creation and setup are asynchronous, potentially with setup scripts, and failures are surfaced. These paths are distinct from merely recording a desired branch name.

PR state is not derived from porcelain output. `GitManager.readRemoteStatus` combines divergence with `lookupStatusPr`/`getPrLookup` and handles a merged-PR fallback; it has its own remote-status result cache. Host integration is behind source-control providers: `packages/source-control-github/src/server/{GitHubSourceControlProvider,GitHubPullRequestProvider,GitHubPullRequestApi,GitHubRest,GitHubCredentials}.ts` implements GitHub lookup/auth/API behavior. GitLab and GitCafe have separate provider packages. `sourceControl/threadPullRequests.ts` tracks thread-linked PRs; a linked PR and the current branch's discovered PR are different concepts. Failure to authenticate must not make a valid local repository look absent.

### Rust port notes

**MUST:** invoke installed `git` with argument vectors, parse NUL-delimited porcelain v2, select the effective checkout, and distinguish no repo/detached/unborn/error/loading. Cache by checkout and refresh on focus, agent completion and git actions; use cancellable background workers. A simple low-frequency local poll is a reasonable PoC simplification. `git2`/`gix` are alternatives, but CLI parity minimizes differences from installed git configuration.

Basic branch switching/creation is MUST if the shown selector is interactive; always expose refusal errors. Recognizing an existing worktree is MUST for correct cwd; creating/cleaning worktrees is SHOULD. PR status lookup and opening its URL are SHOULD for applet fidelity, using `reqwest` plus a defined auth source or an explicitly scoped `gh` adapter. Full PR creation/review/merge/stacks and multi-host support are LATER.

## 6. Project organization, discovery and UI persistence

**Files:** `apps/server/src/orchestration-v2/{ProjectCommands,ProjectStore}.ts`; `packages/contracts/src/orchestrationProject.ts`; `packages/client-runtime/src/state/{projects,threadVisits}.ts`; `apps/web/src/components/Sidebar.tsx`, `apps/web/src/components/Sidebar.drag.ts`, `apps/web/src/components/Sidebar.logic.ts`; `apps/web/src/uiStateStore.ts`; `apps/web/src/hooks/useSidebarProjectGroups.ts`; `apps/server/src/workspace/{KnownWorkspaces,WorkspacePaths,WorkspaceEntries}.ts`; `apps/server/src/project/{ManagedProjectFolders,T3ProjectFileLoader,RepositoryIdentityResolver}.ts`.

A project has ID, title, workspace root, timestamps/deletion state, model/environment defaults, scripts, icon and pin/archive metadata. `ProjectCommands` validates create/update/delete and prevents duplicate active workspace roots. `ProjectStore` states that root matching is by exact string and callers must normalize first. Windows case/slash/path normalization therefore belongs at the boundary, not in a label formatter.

Projects are registered workspaces; the inspected path does not imply automatic scanning of the whole disk for repositories. Known-workspace browsing, folder selection, managed/scratch folders, cloning and repository identity are separate facilities. `T3ProjectFileLoader` loads the project's `t3.json` configuration; setup scripts are an additional workflow, not necessary merely to display a folder in the sidebar. Renaming/deleting a project record must not be conflated with renaming/deleting its directory.

`uiStateStore.ts` persists client preferences in localStorage key `t3code:ui-state:v1`, with a 500 ms debounced writer. It stores project expansion, project order, per-thread local visit timestamps, changed-file disclosure state and related UI preferences. These are not all server project columns. Thread pin/active order keys, on the other hand, have durable schema support (migrations 038/049). `threadVisits.ts` reconciles server `lastVisitedAt` with client fallback timestamps for unread/visited presentation.

**Last-opened distinction:** local visit timestamps and server visit metadata were verified. A dedicated persisted last-opened-thread startup routing key was not verified; do not assert that these timestamps by themselves restore the exact route. `navigationHistory.ts` and `components/sidebar/mainAppLocation.ts` handle navigation concerns, separate from domain persistence.

### Rust port notes

**MUST:** folder picker, normalized workspace roots, persisted CRUD, stable IDs, explicit project/thread ordering and local collapse/selection preferences. Store UI preferences in a small JSON file or dedicated SQLite table, and persist the selected thread directly for deterministic native startup. `rfd` is a possible folder-dialog integration. Avoid canonicalization rules that silently collapse intentionally distinct worktree roots. Repository discovery/cloning, icons, scratch management and `t3.json` setup execution are SHOULD/LATER depending on demo needs.

## 7. Chat rendering, composer and pending work

**Files:** `apps/web/src/components/ChatMarkdown.tsx`; `packages/shared/src/markdownPipeline.ts`; `apps/web/src/markdown-incremental.ts`; `apps/web/src/components/chat/{MessagesTimeline,ThreadTimeline,TimelineV2EventRow,TimelineWorkEntryRow,TimelineAssistantRows,WorkLog,ChatComposer,ComposerModelEffort,ComposerPendingApprovalActions,ComposerPendingUserInputPanel}.tsx`; `apps/web/src/components/chat/{useTimelineRows,useTimelineFollow,useTimelinePositionRestore,useComposerSend,useComposerMenuItems,useComposerAttachmentIntake}.ts`; `apps/web/src/lib/{syntaxHighlighting,incrementalHighlighting,diffRendering}.ts`; `apps/web/src/composerDraftStore.ts`; `apps/server/src/attachmentStore.ts`; `apps/server/src/workspace/WorkspaceSearchIndex.ts`; `apps/server/src/orchestration-v2/QueuedRunOrder.ts`.

Markdown uses `react-markdown` with shared remark/rehype processing: GFM, list normalization, Windows-path preservation, raw HTML handling followed by sanitization, custom file/thread links, media and incremental parsing. Code fences use the Pierre/Shiki highlighting infrastructure (`syntaxHighlighting.ts`), incremental highlighting and a bounded cache in `chat/MarkdownCodeBlock.tsx` (500 entries/50 MiB). `diffRendering.ts` uses `@pierre/diffs`, parses patches and can fall back to raw text when a patch is not renderable. These React/HTML renderers do not transfer to GPUI as widgets.

`MessagesTimeline` uses **LegendList**, stable/incremental row derivation, typed row estimates, remembered scroll positions/disclosures, follow behavior and position restoration. Virtualizing arbitrary-height streaming markdown requires preserving row identity and the user's anchor while content grows. Backend bounded history and frontend virtualization solve different memory/performance problems. Work-log entries are grouped/collapsible, not an unstructured dump of all provider JSON.

Composer internals include provider/model/effort choice, command menus and slash/skill dispatch, mention/context nodes, file search, image/file intake, drafts/stashes, send/stop actions and context attachments. `WorkspaceEntries` routes search through `WorkspaceSearchIndex`; the latter uses an indexed finder and supports both file and content search. A native `@` picker needs a workspace search service, not merely text completion against visible sidebar names.

Attachments have durable server storage and metadata/path resolution; Claude prompt construction converts image attachments and document/file context into provider content/paths. Images must precede the command block for Claude skill expansion. Preserve MIME/size validation, absolute versus workspace-relative identity and persistence across restarts. `ComposerAttachmentStrip.tsx`, `ComposerImageThumbnail.tsx` and `ChatMarkdownMedia.tsx` are the display side. HTTP asset/upload routes matter if retaining Node; an in-process app can expose local image handles instead.

Approval actions and multi-question pending-input panels are distinct from sending another ordinary user message. They answer runtime request IDs and display pending/resolved/interrupted state. `useComposerSend`, command policy and `QueuedRunOrder` distinguish active-turn steering from a queued future run. Queued messages require durable order and cancellation/edit behavior; a local text array outside orchestration is insufficient for restart fidelity. The PoC can initially disallow sending during active work if it clearly communicates that limitation.

### Rust port notes

**MUST:** native rich-text/markdown rendering with selectable/copyable text, fenced code, ordinary links, basic work-log cards, stable virtualized rows, follow/scroll anchoring, composer drafts, send/stop and permission/input responses. `pulldown-cmark` or `comrak` can supply an AST; `syntect` or tree-sitter can supply highlighting. Render GPUI elements from the AST and handle partially closed fences while streaming.

**SHOULD:** attachments/images (`image` for decode, bounded thumbnails), model/effort menus, slash commands, file mentions (`ignore`/`walkdir` and a fuzzy index), queued messages and simple unified diffs (`similar` or parsed git patches). Rich HTML, Mermaid/math, canvas/media apps, speech, citation tooling, elaborate tool-specific cards and side chats are LATER. If an optional feature appears in the composer, its backend becomes required for that shown control.

## 8. Supporting runtime services and scope boundaries

**Checkpoints / turn diffs — SHOULD.** `orchestration-v2/CheckpointService.ts` captures a baseline and result, derives per-turn file summaries and restores checkpoints through `checkpointing/CheckpointStore.ts` → VCS checkpoint operations. `CheckpointRestoreSafety.ts` prevents unsafe restore in shared workspaces. Checkpoint state differs from the repository's current dirty status. Basic branch UI does not require rollback. If adding it, preserve scope/isolation and pre-existing user changes; do not implement undo as indiscriminate `git reset --hard`. Git checkpoint details live below the VCS driver; the exact complete plumbing sequence was not audited here.

**Terminal — LATER.** `apps/server/src/terminal/{Manager,NodePtyAdapter,OutputProtocol}.ts` owns PTY spawning, terminal lifecycle/output and stream flow control; the UI has `components/chat/ChatTerminalDrawers.tsx`. `NodePtyAdapter` wraps `node-pty`, including Windows PID handling. This is separate from Claude's pipe-based SDK process. A Rust terminal would need a PTY layer such as `portable-pty`, a VT parser such as `vte`, resize/input/output and rendering; it is not needed to execute Claude SDK tools.

**Keybindings — MUST for basic editing/send/stop; SHOULD for customization.** `apps/web/src/keybindings.ts`, `components/chat/useChatShortcuts.ts`, `apps/server/src/keybindings.ts` and `config.ts` define routing/configuration. Focus and IME behavior matter: global shortcuts must not steal normal composer input. Native GPUI actions/key contexts can replace web DOM handlers. Command palette and full rebinding editor are LATER.

**Startup and process supervision — MUST.** `server.ts`, `bootstrap.ts`, desktop backend manager/pool and `shell/DesktopShellEnvironment.ts` establish backend availability and shell-derived environment. Initialize storage/settings before selecting the restored thread, discover Claude/git, surface unavailable/login failures, then start subscriptions. A retained Node sidecar requires owned-process startup, readiness, crash/error handling and orderly shutdown.

**Logging — MUST at a minimal level.** `config.ts` defines logs/provider logs; `apps/server/src/observability/` and Claude protocol-event logging provide diagnostics. Use structured Rust `tracing` with thread/run/request IDs, bounded files and stderr capture. Preserve privacy: the Claude runner deliberately avoids placing arbitrary startup stderr into public turn failures. Full telemetry/export and upstream metrics are LATER.

**Notifications — SHOULD.** `components/chat/ThreadActivityAnnouncer.tsx` and sidebar thread indicators show activity/accessibility state. Unread/visited tracking is already part of shared thread state. Native completion/approval attention can use an OS notification bridge (for example `notify-rust` where supported), while ordinary in-app failure/approval indication is MUST. Full upstream desktop notification plumbing was not traced, so no exact OS-notification parity is claimed.

## Final prioritized implementation table

Complexity estimates are relative to this PoC: S = contained component/service; M = multiple components and lifecycle concerns; L = protocol/state-machine or substantial platform work. Node retention changes estimates as noted.

| Feature | Priority | Upstream files | Port complexity |
|---|---|---|---|
| Typed background command/event boundary | MUST | `packages/contracts/src/rpc.ts`; `apps/server/src/ws.ts`; `packages/client-runtime/src/rpc/session.ts` | M in-process; L for Effect RPC parity |
| Project/root CRUD and duplicate/path handling | MUST | `orchestration-v2/ProjectCommands.ts`, `ProjectStore.ts`; `client-runtime/state/projects.ts` | M |
| Thread/project ordering and collapse/selected-thread preferences | MUST | `apps/web/src/uiStateStore.ts`; `Sidebar.drag.ts`; migrations 038/049 | M |
| SQLite schema, migrations and settings bootstrap | MUST | `persistence/Migrations.ts`; `config.ts`; `serverSettings.ts` | M with own schema; L for import compatibility |
| Durable thread/run/session/message/activity state | MUST | `Orchestrator.ts`; `EventSink.ts`; `ProjectionStore.ts`; contracts `orchestrationV2.ts` | L |
| Incremental read models and bounded/paged history | MUST | `WireProjection.ts`; `LiveStreamBudget.ts`; `orchestrationV2Projection.ts` | M |
| Command deduplication, stale-output guards and crash reconciliation | MUST | `CommandReceiptStore.ts`; `ThreadCommandExecutor.ts`; `EffectOutbox.ts` | M/L |
| Claude SDK sidecar, resume and interruption | MUST | `Adapters/ClaudeAdapterV2.ts`; `ProviderSessionManager.ts` | L (reduced by retaining SDK) |
| Windows executable/env/owned-process lifecycle | MUST | `Drivers/ClaudeExecutable.ts`, `ClaudeHome.ts`; SDK spawn callback | M |
| Permission and pending-question continuations | MUST | `ClaudeAdapterV2.canUseToolEffect`; `RuntimePolicy.ts`; composer pending panels | L |
| Basic model selection and provider errors | MUST | `claudeModelOptions.ts`; `ClaudeModelCatalog.ts`; `useComposerProviderModel.ts` | M |
| Markdown/code, stable timeline, composer draft/send/stop | MUST | `ChatMarkdown.tsx`; `MessagesTimeline.tsx`; `ChatComposer.tsx`; `composerDraftStore.ts` | L |
| Basic structured tool/work-log entries | MUST | `ClaudeAdapterV2.ts`; `TimelineWorkEntryRow.tsx`; `WorkLog.tsx` | M |
| Git current branch/dirty/divergence and effective checkout | MUST | `GitVcsDriverCore.ts`; `VcsStatusBroadcaster.ts`; `GitActionsControl.tsx` | M |
| Focus/turn/action invalidation and status cache | MUST | `apps/web/src/state/vcs.ts`; `RunFinalizationService.ts`; `VcsStatusBroadcaster.ts` | M |
| Branch create/switch with errors and occupancy checks | MUST if interactive selector shown | `BranchToolbarBranchSelector.tsx`; `GitVcsDriverCore.ts` | M |
| Minimal startup supervision, keybindings and diagnostics | MUST | desktop backend manager; `bootstrap.ts`; `keybindings.ts`; `observability/` | M |
| Images/files, durable attachment storage | SHOULD | `attachmentStore.ts`; `useComposerAttachmentIntake.ts`; `ChatMarkdownMedia.tsx` | M |
| File mentions/search and slash/skill commands | SHOULD | `WorkspaceSearchIndex.ts`; `useComposerMenuItems.ts`; `Drivers/ClaudeSkillDispatch.ts` | M |
| Queued messages and steering | SHOULD | `QueuedRunOrder.ts`; `CommandPolicy.ts`; `useComposerSend.ts` | M |
| Effort/thinking, plan mode and plan approval | SHOULD | `claudeModelOptions.ts`; Claude runtime query policy; `ComposerModelEffort.tsx` | M/L |
| Token/context usage and retry/limit presentation | SHOULD | `ClaudeTurnTokenUsage.ts`; `ClaudeAdapterV2.ts` | M |
| PR status/auth/cache and open URL | SHOULD | `GitManager.ts`; `source-control-github/src/server/GitHubPullRequestProvider.ts` | M |
| Worktree creation/setup/cleanup | SHOULD | `GitVcsDriverCore.ts`; `WorktreeBranchSync.ts`; `project/ProjectSetupScriptRunner.ts` | L |
| Checkpoints, turn diffs and safe restore | SHOULD | `CheckpointService.ts`; `CheckpointRestoreSafety.ts`; `checkpointing/CheckpointStore.ts` | L |
| Completion/attention notifications | SHOULD | `ThreadActivityAnnouncer.tsx`; `ThreadStatusIndicators.tsx`; `threadVisits.ts` | S/M |
| Embedded terminal and shell-session persistence | LATER | `terminal/Manager.ts`, `NodePtyAdapter.ts`; `ChatTerminalDrawers.tsx` | L |
| Full PR workflows, stacks and multi-host source control | LATER | `GitActionsControl.tsx`; `packages/source-control-*` | L |
| Remote/multi-environment auth, relay, mobile | LATER | `ws.ts`; `auth/`; `client-runtime/connection/`; desktop backend pool | L |
| Delegation, schedules, handoff, rich media apps and legacy import | LATER | orchestration V2 subagent/context services; migrations; chat canvas/media components | L |

Paths abbreviated to a filename in this table refer to the fully qualified subsystem paths cited above; server orchestration names default to `apps/server/src/orchestration-v2/`. Optional SHOULD items become MUST if their controls or outputs are explicitly included in the visible PoC acceptance criteria.
