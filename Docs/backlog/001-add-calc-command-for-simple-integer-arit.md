---
id: 1
title: Add `calc` command for simple integer arithmetic
key: llm:add-calc-command-for-simple-integer-arit
kind: feature
priority: 2
status: done
created: 2026-11-14T15:48:32Z
updated: 2026-11-14T16:06:59Z
branch: evolve/001-add-calc-command-for-simple-in
shipped: 2
signals: [{"key": "eb01af91d2ffc7b5", "kind": "missed", "text": "calc", "t": "2026-11-14T15:46:49Z", "build": "1", "cmd": null}, {"key": "cda74443a2d2b476", "kind": "wish", "text": "a calc command for simple calculations like 2 + 3 or 2 * 3", "t": "2026-11-14T15:47:42Z", "build": "1", "cmd": null}]
---
# #1 Add `calc` command for simple integer arithmetic

## Summary
Users tried to run `calc`, which doesn't exist yet, and one user explicitly asked for a calc command for simple calculations like `2 + 3` or `2 * 3`. Add a BeeShell `calc EXPR` command that evaluates integer expressions with +, -, *, / and % plus parentheses, using normal operator precedence. It should also accept a unary minus. Use i64 arithmetic with checked operations so that division by zero and overflow print an error and never panic the kernel. List the command in `help`.

## Acceptance
- `calc 2 + 3` prints `5` and `calc 2 * 3` prints `6`
- Spacing doesn't change the result: `calc 2+3*4` prints `14` and `calc (2 + 3) * 4` prints `20`
- `calc -7 / 2` and `calc 7 % 3` give consistent integer results (documented truncation toward zero)
- `calc 1 / 0` prints an error like `calc: division by zero` and the kernel keeps running
- An i64 overflow (e.g. `calc 9223372036854775807 + 1`) prints `calc: overflow` and does not panic
- Bad input (`calc 2 +`, `calc abc`, `calc` with no arguments) prints a usage or parse error and does not panic
- `help` lists `calc EXPR`
- Add unit tests for the expression evaluator covering precedence, parentheses, unary minus, division by zero, overflow and parse errors
- Add a selftest/integration test case that runs `calc 2 + 3` in the shell and checks that the output is `5`
- reviewer: first attempt ran on a commit without the test infrastructure

## Signals (2)
- 2026-11-14T15:46:49Z b1 missed: calc
- 2026-11-14T15:47:42Z b1 wish: a calc command for simple calculations like 2 + 3 or 2 * 3

## Log
- 2026-11-14T15:48:32Z created from 2 signals
- 2026-11-14T15:48:32Z agent claude-code started on evolve/001-add-calc-command-for-simple-in (from main 7bb9e71)
- 2026-11-14T15:48:52Z needs attention: the agent changed nothing; no test added or changed; `make check` failed (exit 2) (0 files, 0 lines)
- 2026-11-14T15:58:24Z attempt rejected: first attempt ran on a commit without the test infrastructure (back to open)
- 2026-11-14T16:04:38Z agent claude-code started on evolve/001-add-calc-command-for-simple-in (from main f8564ea)
- 2026-11-14T16:06:25Z ready for review (4 files, 176 lines)
- 2026-11-14T16:06:59Z approved and merged; shipped in build b2
