# Schematic suite — baseline on the old YAML path (2026-09-01)

`python3 quality/run.py --suite schematic`, `gpt-5.6-luna`, kicad-cli 9.0.2, before the
live-edit rewrite lands. The agent still works through `create_design` / `edit_design` /
`apply_design`, so this is the number the rewrite has to beat.

| case                  | score | checks | erc e/w | moved | lost | added | elapsed |
| --------------------- | ----- | ------ | ------- | ----- | ---- | ----- | ------- |
| sch-create-large      | 0     | 0/7    | ?/?     | -     | -    | -     | 176s    |
| sch-create-medium     | 3     | 6/7    | 0/0     | -     | -    | -     | 114s    |
| sch-create-small      | 3     | 6/7    | 0/0     | -     | -    | -     | 53s     |
| sch-extend-led        | 3     | 8/10   | 0/0     | 0     | 0    | 13    | 117s    |
| sch-extend-protection | 0     | 7/9    | 0/37    | 0     | 0    | 0     | 12s     |
| sch-extend-testpoints | 0     | 7/10   | 0/2     | 0     | 0    | 5     | 175s    |
| sch-replace-connector | 1     | 7/10   | 0/32    | 0     | 0    | 0     | 12s     |
| sch-replace-ic        | 2     | 5/7    | 0/37    | 0     | 0    | 0     | 13s     |
| sch-replace-passive   | 2     | 7/9    | 0/37    | 0     | 0    | 0     | 12s     |

`?/?` means ERC was never run: there was no schematic to check. Nine cases, nine failures.

## Create: it builds, it undershoots

`sch-create-small` and `-medium` produce a clean schematic — ERC 0/0, and the extractor's
net partition equals `kicad-cli`'s — but they build too little of what was asked: 7 parts
against a floor of 8, 9 against 18. `sch-create-large` never writes a schematic at all and
the turn exits 1, so every downstream fact is *not measured* and every check fails closed.

## Replace and extend: the old path cannot open a schematic it did not write

Four of the six edit cases end with `agent_exit == 1` and the file byte-identical to the
input. The transcript is the same each time — the model renders the schematic, has no tool
that can reach into it, and the turn aborts:

    error: agent turn ended without completing its quality contract: NoProgress { completions: 3 }

That is the honest baseline for *"replace a single component in a given project"* and
*"build upon a given schematic"*: **did not complete**. There is nothing to grade beyond it.

The two cases where the old path did act show the failure mode the rewrite exists to fix:

- **`sch-extend-testpoints`** deleted the design and rebuilt it. All 25 symbols removed, 5
  added, and every one of the nine original nets gone from the partition — `net_delta_removed`
  lists `GND`, `Net-(P1-PM)`, `Net-(P2-P1)`, `Net-(P3-P1)`, `Net-(P4-P1)`, `Net-(P4-PM)`,
  `Net-(U1A-G)`, `Net-(U1A-K)`, `Net-(U1B-K)`.
- **`sch-extend-led`** kept the parts but rewrote the file: 17 of 26 symbols come back with a
  new UUID (`symbols_reidentified`), all 8 power symbols are dropped and 13 symbols appear
  where at most 5 were asked for.

`unchanged_symbols_moved` and `fields_lost` read 0 across the board, which is not a pass: on
the four aborted cases nothing was touched, and on the two that rewrote the file the parts
were destroyed and recreated rather than moved, so they land in `symbols_removed` /
`symbols_reidentified` instead. Those two columns become meaningful once the agent edits in
place; until then `symbols_removed`, `symbols_reidentified` and `net_delta_removed` are the
columns that carry the signal.

## What "fixed" looks like

The rewrite should turn every edit case green on: `agent_exit == 0`,
`unchanged_symbols_moved == []`, `fields_lost == []`, `symbols_removed == []`,
`net_delta_removed == []`, plus each case's positive assertion (`fields_changed`,
`lib_ids_changed`, `symbols_added`). Create cases need `part_count` to reach the floor with
ERC clean and the partition still matching `kicad-cli`.

## Reproducing

    tools/sch_e2e.sh --output quality/runs --scoreboard quality/baselines/<date>-schematic.md

Cases run sequentially against the real gateway; the whole suite is roughly 15 minutes when
the machine is otherwise idle. Timings here were taken under load from parallel builds and
are indicative only.
