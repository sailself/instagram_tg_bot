# CLAUDE.md

The full agent guide for this repo lives in **@AGENTS.md** — read it before
working here. (Claude Code auto-loads `CLAUDE.md`; the import above pulls in
`AGENTS.md` so the two never drift.)

Two things that are easy to miss and **must be respected**:

1. **Agent Execution Logging** — every substantial implementation/investigation
   request gets a log under `agent_logs/` (`<YYYYMMDD_HHMMSS>_<task>.md`). See the
   "Agent Execution Logging" section in @AGENTS.md for the required contents.
2. The **landmine invariants** in @AGENTS.md (UTF-16 caption lengths, verbatim
   CDN query strings, 2–10-item media groups, `panic = "abort"`, cookieless-first,
   host-parsed `is_ig_cdn`). Violating any of these silently breaks delivery.
