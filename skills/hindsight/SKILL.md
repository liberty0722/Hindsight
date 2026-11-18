---
name: hindsight
description: Search and read past coding-agent sessions on this machine. Use when the user refers to earlier conversations, decisions, why something was done, where work stopped, or whether an error was seen before.
---

# Hindsight — past coding-agent sessions

Hindsight indexes every coding-agent session on this machine — Claude Code, Codex, Cursor,
Gemini CLI, OpenCode and a dozen more — and `hindsight-cli` reads that index from a shell.

Reach for it whenever the answer lives in an earlier conversation rather than in the
code: what was discussed, what was decided and why, where previous work stopped, which
approaches were already tried and rejected, whether an error has been seen before. Git
history and the working tree do not hold any of that.

Everything below is read-only. Nothing here can modify or delete a session.

## Finding the binary

On Windows (PowerShell):

```powershell
(Get-Command hindsight-cli -ErrorAction SilentlyContinue).Source
```

It is `hindsight-cli.exe` next to `Hindsight.exe` in the folder the user unzipped; if it is not on
`PATH`, ask the user for that folder and call it by full path (`& 'C:\…\hindsight-cli.exe' …`).

On macOS and Linux:

```bash
command -v hindsight-cli || ls /Applications/Hindsight.app/Contents/MacOS/hindsight-cli
```

Use whichever resolves. On Linux it is on `PATH` after a deb install, or at
`~/.local/bin/hindsight-cli` from the tarball.

Two things can be missing, and they need different answers:

- **No binary.** Say so — the user needs [Hindsight](https://github.com/liberty0722/Hindsight).
  Do not try to install it yourself.
- **No index** (`no Hindsight index at … — launch Hindsight once to build it`). Hindsight is
  installed but has never run. Run `hindsight-cli index` once; it builds the index in a
  few seconds without opening the app, then retry your query.

If the freshness line at the end of a listing is well behind and Hindsight is not running,
`hindsight-cli refresh` updates the index in a few seconds; it does nothing while Hindsight has its
window open or is still scanning. Retry the query afterwards.

## Commands

Always scope to the current repository with `--project "$PWD"` unless the user clearly
means "everywhere". Without it, every project on the machine is in scope.

**What happened in this project recently**

```bash
hindsight-cli sessions --project "$PWD" --limit 10
```

Add `--agent codex` (repeatable), `--since 7d`, or `--starred` to narrow it.

**Where was this discussed**

```bash
hindsight-cli search "rate limiter" --project "$PWD"
```

Terms are ANDed. CJK text and code substrings like `useEffect(` work. Drop `--project`
when the user asks about something they might have worked on elsewhere.

**Read a session**

```bash
hindsight-cli show 'claude-code:1b2c3d4e-…'
```

Add `--tools` for tool inputs and outputs, `--thinking` for recorded reasoning. Both are
verbose — start without them. Work done by subagents is not in the main transcript: each
subagent is a session of its own, listed at the end of the output by key — `sessions` only
lists parents, so this is where to find them. Claude Code and Cursor subagents can also be
read through the parent with `--subagent ID`, or `--subagent '*'` to list their ids.

**Which projects have history**

```bash
hindsight-cli projects --limit 20
```

**What an agent already remembers about this project**

```bash
hindsight-cli memories --project "$PWD"
```

The memory files agents keep for themselves — Claude Code's auto-memory, Codex's
memories, ZCode's project memory — plus the instruction files kept for them (CLAUDE.md,
AGENTS.md, GEMINI.md, `.cursor/rules` …), read-only, grouped by project, user memory
(notes that apply to every project) last. Each entry ends with a `hindsight://memory/…`
reference; pass it to `show` to read the file. Reach for it before re-deriving a
project's conventions or decisions from scratch.

## Keys, references and paging

`sessions` and `search` print a key for every session — `<agent>:<id>`, or
`<agent>:<host>:<id>` for a session mirrored from a remote machine. Pass a key to `show`.

`search` also prints `ref: hindsight://session/<key>#<seq>` under each snippet. Pass the whole
reference (quoted) to `show` and the transcript starts at that message — that is the fast
path from "this term was mentioned" to "here is the surrounding conversation".

Long transcripts are paginated. When the output ends with a `from_seq` hint, run again
with `--from <that number>` to continue.

## Reading the results

- Exit code `0` means it ran — **including "no matches" and "no such project"**. Do not
  treat an empty result as a failure; report that nothing was found.
- Exit `1` means it ran and failed — an unknown or ambiguous key, or a transcript that
  would not parse. Exit `2` means the command line was wrong, or there is no readable index
  yet. Both print an explanation to stderr.
- Listings usually end with the index freshness (`Index covers activity up to …`) — the
  newest activity Hindsight has indexed, not the current time. A stale-looking value usually means
  Hindsight is not running, but some agents (Copilot, OpenCode, Antigravity, Hermes, OpenClaw) are
  stored in SQLite and only refresh when Hindsight launches, on its Refresh button or on
  `hindsight-cli refresh`, so say what the line shows rather than
  concluding the user did nothing. `show` does not depend on that line — it reads
  the agent's own file rather than the index. The exception is a session from a remote
  host (`<agent>:<host>:<id>`): that file is a local mirror, current as of Hindsight's last
  successful sync. Say so rather than reporting it as the other machine's latest state.
- Values out of range are clamped, not rejected: `--limit 999` returns the maximum.

## Sensible defaults

- Start with `sessions --project "$PWD"` when the user says "last time" or "yesterday",
  and with `search` when they name a topic, an error or a decision.
- Read one or two transcripts, not ten — `show` output is large.
- Quote keys and `hindsight://` references: they contain characters the shell will eat.
