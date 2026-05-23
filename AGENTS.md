# AGENTS.md

## Principles

- Keep the system modular.
- Prefer composition over monolithic services.
- Avoid tight coupling between monitoring and notification layers.
- Every feature must be testable independently.

## Forbidden

- No business logic inside notifiers.
- No shell execution inside collectors.
- No global mutable state.
- No blocking IO inside async Tokio tasks.
- No unwrap() in production code.

## Tooling

- Rust stable only
- cargo fmt
- clippy
- rust-analyzer
- mypy/ruff equivalents are mandatory for Python scripts
- uv only for Python tooling
- no pip

## Architecture Rules

- collectors only collect data
- alerts only evaluate rules
- notifiers only dispatch events
- services orchestrate workflows

## Validation

Before every PR or generation:

- cargo fmt --check
- cargo clippy --all-targets --all-features
- cargo test