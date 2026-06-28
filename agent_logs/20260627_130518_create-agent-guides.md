# Create CLAUDE.md and AGENTS.md (+ logging convention)

## User request

> now you have a good understanding of the whole project. create CLAUDE.md and
> AGENTS.md for the project. Adding one more guideline that's must be respected:
> [Agent Execution Logging spec — log every substantial request under
> `agent_logs/` as `<YYYYMMDD_HHMMSS>_<task>.md` with prompt, goal, plan, actions,
> validation, follow-ups, and optional design-decisions/deviations/tradeoffs/
> open-questions; no secrets].

## Goal

Give future agents a concise, high-signal guide to the repo, and bake in the
mandatory Agent Execution Logging convention.

## Plan used

- Make `AGENTS.md` the canonical guide; have `CLAUDE.md` import it via `@AGENTS.md`
  so the two can't drift.
- Capture the hard-won invariants (the review landmines) so future agents don't
  reintroduce them.
- Establish `agent_logs/` by writing this session's logs.

## Actions completed

- Wrote **`AGENTS.md`**: project overview; pointers to `docs/PLAN.md` &
  `docs/CHECKLIST.md`; build/test commands; architecture mental model; the
  "invariants & gotchas" landmine list (UTF-16 lengths, verbatim CDN query
  strings, 2–10 media groups, 50 MB cap, `panic=abort`, host-parsed `is_ig_cdn`,
  cookieless-first, memory discipline, graceful-failure rule, dedup semantics);
  conventions; deployment; verification; and the verbatim **Agent Execution
  Logging** section.
- Wrote **`CLAUDE.md`**: imports `@AGENTS.md` and re-flags the logging rule and
  landmines as must-respect.
- Created **`agent_logs/`** with the retroactive implementation log and this file.

## Validation

Docs-only change; no code impact. `cargo test` / build unaffected. Verified the
files were written and the logging spec is reproduced verbatim in `AGENTS.md`.

## Follow-ups / TODOs

- If these files are committed, include `agent_logs/` (currently tracked — not
  gitignored).

## Design decisions

- **`CLAUDE.md` imports `AGENTS.md`** rather than duplicating content — DRY, and
  Claude Code resolves `@AGENTS.md`. Other tools read `AGENTS.md` directly, so
  both audiences are covered by one source of truth.

## Open questions

- Prefer two fully standalone (duplicated) files instead of the import pattern?
- Should `agent_logs/` be committed to the repo or gitignored as local-only?
