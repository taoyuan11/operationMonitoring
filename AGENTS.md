# Repository Guidelines

## Project Structure & Module Organization

This repository contains three applications:

- `backend/`: Rust/Axum API, PostgreSQL persistence through SQLx, WebSockets, alerting, agent updates, and admin authentication. General HTTP handlers live in `backend/src/handlers/`; larger domains such as alerts, audit, Docker, files, remote desktop, and updates have top-level modules. Shared state and database code are in `state.rs` and `db.rs`.
- `instanceEnd/`: Rust agent for host metrics and inventory, command execution, Docker and file operations, lifecycle management, self-updates, PTY/ConPTY terminals, and Windows remote desktop.
- `front-end/`: Vue 3, TypeScript, and Vite console. Use `src/api/` for HTTP wrappers, `src/components/` for UI, `src/composables/` for workflows, `src/types/` for domain types, and `src/styles/` for CSS. Static assets belong in `public/`; Node test files belong in `tests/`.

Keep protocol or model changes synchronized across the backend, agent, and frontend consumers.

## Build, Test, and Development Commands

Run commands from the relevant module directory:

```bash
cd backend && OM_DATABASE_PASSWORD='<database-password>' OM_ADMIN_PASSWORD=development-bootstrap-password cargo run  # API on :13500
cd instanceEnd && cargo run -- start --server http://127.0.0.1:13500
cd front-end && pnpm install && pnpm dev  # Vite dev server on 127.0.0.1:5173
```

The backend requires a reachable PostgreSQL server. Set `OM_DATABASE_URL` when the local database does not match the default URL documented in `README.md`. The agent `start` command launches it in the background; use `cargo run -- stop` when the development check is complete.

Before submitting changes, run:

```bash
cd backend && cargo fmt --check && cargo test && cargo check
cd instanceEnd && cargo fmt --check && cargo test && cargo check
cd front-end && pnpm test && pnpm build
```

`pnpm test` runs the frontend's Node-based regression tests, and `pnpm build` type-checks before the production build. Run `cargo build --release` in `instanceEnd/` for a deployable agent.

Any server or long-running process started for testing, development, or verification must be stopped as soon as that check is complete. Before handing work back to the user, verify that every process started during the task has been terminated; never leave a development server, backend, agent, watcher, or preview process running unless the user explicitly asks for it to remain available.

## Coding Style & Naming Conventions

Rust uses `rustfmt`: four-space indentation, `snake_case` modules/functions, and `PascalCase` types. Prefer typed errors and existing shared models.

Vue components use PascalCase filenames (for example, `InstanceBoard.vue`), `<script setup lang="ts">`, two-space indentation, single quotes, and no semicolons. Prefix composables with `use`, keep domain interfaces in `types/domain.ts`, and use kebab-case CSS classes. TypeScript rejects unused symbols and switch fallthrough.

## Testing Guidelines

Rust tests are inline `#[cfg(test)]` modules; name them after behavior, such as `accepts_global_options_after_subcommand`. Use `#[tokio::test]` for async behavior and add regressions beside changed logic. PostgreSQL-backed ignored tests must use a dedicated empty database through `OM_TEST_DATABASE_URL` and run serially as documented in `backend/README.md`. Frontend regression tests use the Node test runner under `front-end/tests/`; add focused tests for extractable logic, run `pnpm test`, and manually check UI changes at narrow and wide viewports.

## Commit & Pull Request Guidelines

History follows scoped Conventional Commits, such as `feat(frontend): ...`. Use an imperative subject and a relevant scope (`backend`, `frontend`, `agent`, or `terminal`); separate unrelated changes.

Pull requests should explain behavior and architecture impact, list verification commands, link issues, and include screenshots for UI changes. Highlight schema, API, WebSocket, or configuration changes.

## Security & Configuration

Never commit passwords, `.env` files, PostgreSQL dumps, logs, authentication keys, update-signing keys, or agent identity files. Use a unique `OM_ADMIN_PASSWORD` of at least 16 bytes and never retain the public example value. Document new `OM_*` variables in `README.md`, and keep generated uploads, update artifacts, and runtime data out of source control.
