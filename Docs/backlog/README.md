# Backlog

Work for the improvement loop (`tools/evolve/evolve.py`, README phase 7).
Items are created from what the OS records: wishes, feedback, commands that
don't exist, crash reports and failed A/B trials.

## Files

| File | What |
|---|---|
| `NNN-title.md` | one backlog item (front matter + summary, acceptance criteria, signals, log) |
| `reviews/NNN.md` | the review report of the latest attempt: verdict, `make check`, diff size, tests, kernel size, the agent's summary |
| `seen.json` | signals already turned into items (so they are not added twice) |

Item status: `open` → `working` (agent running) → `review` → `done` (merged,
with the build it shipped in) or `rejected`. `reject N --retry` puts an item
back to `open`, adding the reason to its acceptance criteria.

## Commands

```sh
python3 tools/evolve/evolve.py status
python3 tools/evolve/evolve.py harvest [--disk build/beecoders.img]   # new signals -> items
python3 tools/evolve/evolve.py work [N]                              # coding agent (commit your work first)
python3 tools/evolve/evolve.py review N
python3 tools/evolve/evolve.py approve N [--deploy]
python3 tools/evolve/evolve.py reject N --reason "..." [--retry]
```

You can also write an item by hand: copy an existing file, give it the next
number, set `status: open` and describe the change and its acceptance criteria.
Edit an item's priority (1 = most urgent) to change the order.
