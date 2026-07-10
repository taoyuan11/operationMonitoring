# Repository Guidelines

## Project Structure & Module Organization

This repository contains three applications:

- `backend/`: Rust/Axum API, SQLite persistence, WebSockets, and admin authentication. HTTP handlers live in `backend/src/handlers/`; shared state and database code are in `state.rs` and `db.rs`.
- `instanceEnd/`: Rust agent for host metrics, command execution, lifecycle management, and PTY/ConPTY terminals.
- `front-end/`: Vue 3, TypeScript, and Vite console. Use `src/api/` for HTTP wrappers, `src/components/` for UI, `src/composables/` for workflows, `src/types/` for domain types, and `src/styles/` for CSS. Static assets belong in `public/`.

Keep protocol or model changes synchronized across the backend, agent, and frontend consumers.

## Build, Test, and Development Commands

Run commands from the relevant module directory:

```bash
cd backend && OM_ADMIN_PASSWORD=admin123 cargo run  # API on :13500
cd instanceEnd && cargo run -- log --server http://127.0.0.1:13500
cd front-end && pnpm install && pnpm dev             # Vite dev server
```

Before submitting changes, run:

```bash
cd backend && cargo fmt --check && cargo test && cargo check
cd instanceEnd && cargo fmt --check && cargo test && cargo check
cd front-end && pnpm build
```

`pnpm build` type-checks before the production build. Run `cargo build --release` in `instanceEnd/` for a deployable agent.

Any server or long-running process started for testing, development, or verification must be stopped as soon as that check is complete. Before handing work back to the user, verify that every process started during the task has been terminated; never leave a development server, backend, agent, watcher, or preview process running unless the user explicitly asks for it to remain available.

## Coding Style & Naming Conventions

Rust uses `rustfmt`: four-space indentation, `snake_case` modules/functions, and `PascalCase` types. Prefer typed errors and existing shared models.

Vue components use PascalCase filenames (for example, `InstanceBoard.vue`), `<script setup lang="ts">`, two-space indentation, single quotes, and no semicolons. Prefix composables with `use`, keep domain interfaces in `types/domain.ts`, and use kebab-case CSS classes. TypeScript rejects unused symbols and switch fallthrough.

## Testing Guidelines

Rust tests are inline `#[cfg(test)]` modules; name them after behavior, such as `accepts_global_options_after_subcommand`. Use `#[tokio::test]` for async behavior and add regressions beside changed logic. The frontend has no test runner, so `pnpm build` is mandatory; manually check UI changes at narrow and wide viewports.

## Commit & Pull Request Guidelines

History follows scoped Conventional Commits, such as `feat(frontend): ...`. Use an imperative subject and a relevant scope (`backend`, `frontend`, `agent`, or `terminal`); separate unrelated changes.

Pull requests should explain behavior and architecture impact, list verification commands, link issues, and include screenshots for UI changes. Highlight schema, API, WebSocket, or configuration changes.

## Security & Configuration

Never commit passwords, `.env` files, SQLite databases, logs, or agent identity files. Override the development-only `admin123` password. Document new `OM_*` variables in `README.md`, and keep generated uploads and runtime data out of source control.
