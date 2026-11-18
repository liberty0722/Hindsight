# Hindsight CLI (`hindsight-cli`)

`hindsight-cli` is a small, read-only command-line tool that ships with Hindsight. It answers the same three questions the [MCP server](mcp.md) answers — what did I discuss, what did I work on here, what happened in that session — from a terminal, a script, or any agent that can run a shell command.

It prints what an MCP client is given: the two surfaces call the same code, and the test suite asserts their output byte for byte apart from a trailing newline the CLI adds (see [Output](#output)). So a `hindsight-cli show …` transcript and a `hindsight_get_session` reply are the same text, and this page and [docs/mcp.md](mcp.md) describe one format, not two.

Nothing here can modify a session. The CLI opens Hindsight's index without write access and never touches the agents' own files except to read a transcript. Two commands write to Hindsight's own index, and only while the app is closed: [`hindsight-cli index`](#index) builds one when there is none yet, and [`hindsight-cli refresh`](#refresh) brings an existing one up to date. Neither ever rebuilds an index — that stays the app's job.

## Where it lives

| Platform | Path |
|---|---|
| Windows | `hindsight-cli.exe`, next to `Hindsight.exe` in the folder you unzipped (e.g. `C:\Tools\Hindsight\hindsight-cli.exe`) |
| macOS | `/Applications/Hindsight.app/Contents/MacOS/hindsight-cli` |
| Linux | next to `hindsight`: `~/.local/bin/hindsight-cli` (tar.gz install) or `/usr/bin/hindsight-cli` (deb) |

On Windows, add that folder to your user `PATH` once from PowerShell (no admin rights needed), then open a new terminal:

```powershell
[Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "User") + ";C:\Tools\Hindsight", "User")
```

Or use *Settings* → *System* → *About* → *Advanced system settings* → *Environment Variables* → *Path* (user) → *New*. If you would rather not touch `PATH`, the full path works anywhere: `& 'C:\Tools\Hindsight\hindsight-cli.exe' sessions`.

On macOS it lives inside the app bundle, so link it once:

```bash
sudo mkdir -p /usr/local/bin && sudo ln -sf '/Applications/Hindsight.app/Contents/MacOS/hindsight-cli' /usr/local/bin/hindsight-cli
```

On Linux the deb puts it in `/usr/bin`; the tar.gz installer puts it in `~/.local/bin`, which you may need to add to your `PATH`.

`hindsight-cli setup` prints the path for your install, how to put it on your `PATH`, and a block you can paste into a project's `CLAUDE.md` / `AGENTS.md` to teach an agent to use it. Updating Hindsight in place (unzipping the new version over the same folder) keeps the path, so there is nothing to redo after an update.

The binary is not named `hindsight`: that name already belongs to the GUI. On Windows you can give it a shorter name with a PowerShell alias in your `$PROFILE` (`Set-Alias hs hindsight-cli`); elsewhere, symlink it.

## Commands

```bash
hindsight-cli search QUERY [OPTIONS]
hindsight-cli sessions [OPTIONS]
hindsight-cli show KEY [OPTIONS]
hindsight-cli projects [OPTIONS]
hindsight-cli memories [OPTIONS]
hindsight-cli setup
hindsight-cli context
hindsight-cli index
hindsight-cli refresh
hindsight-cli --help | --version
```

Global options, valid anywhere on the line: `--db PATH` (another index database), `--help` / `-h`, `--version` / `-V`, and `--` to end option parsing. Command options must come after their command. Every option that takes a value also accepts the `--flag=value` form, which is how you pass a value that starts with a dash. The switches (`--starred`, `--tools`, `--thinking`) take no value at all.

There is no default project scope. Pass `--project "$PWD"` when you mean "here" — otherwise every project on the machine is in scope. A relative path (`.`, `../x`) is refused rather than guessed at, because inside a symlinked checkout the shell's `$PWD` and the process's working directory disagree.

### `search`

Full-text search across every indexed session — titles, user prompts, assistant replies, tool names and inputs. A title match is listed as `title` with a reference to the start of the session. Terms are ANDed; CJK text and code substrings like `useEffect(` work. Returns matching sessions with up to three snippets each, and a `hindsight://session/<key>#<seq>` reference per snippet.

| Option | Value | Notes |
|---|---|---|
| `--project` | path or name | absolute path, or a project name |
| `--agent` | agent id | repeatable, or comma-separated |
| `--since` | when | see [When](#when) |
| `--limit` | N | default 10, max 30 |

```bash
hindsight-cli search "rate limiter" --project "$PWD" --since 7d
hindsight-cli search "useEffect(" --agent codex --agent claude-code
hindsight-cli search -- --dry-run          # a query that starts with a dash
```

### `sessions`

The most recently updated sessions. Sub-agents (Claude Code and Cursor subagents, Codex `spawn_agent`, Grok, OpenCode, DeepSeek Harness and others) are sessions of their own, folded into their parents — `show` on the parent lists them by key; Codex's background threads (auto-review, `/review`, compaction, memory consolidation) are not indexed at all. Archived sessions are excluded. This is what to run when you want the key of "the session from yesterday".

Same options as `search`, plus `--starred` (only sessions you starred in Hindsight). `--limit` defaults to 20, max 100.

```bash
hindsight-cli sessions --project "$PWD" --limit 5
hindsight-cli sessions --agent codex --since 24h
```

### `show`

One session's transcript, parsed live from the agent's own files, as compact Markdown: user and assistant messages with `[seq N]` markers, tool calls folded to one line each, injected context omitted. Because it reads the file rather than the index, it does not depend on when Hindsight last scanned. One exception: a session mirrored from a remote host is read from the local mirror, so it reflects Hindsight's last successful sync rather than the remote machine right now.

| Option | Value | Notes |
|---|---|---|
| `--from` | SEQ | start at this message (min 0); a `hindsight://…#N` reference sets it too, and an explicit `--from` wins over the reference's seq |
| `--messages` | N | messages per page, default 60, range 1–200 |
| `--chars` | N | character budget per page, default 20000, range 200–100000 |
| `--message-chars` | N | truncate each message, default 4000, range 100–50000 |
| `--tools` | — | include tool inputs and outputs (verbose) |
| `--thinking` | — | include the assistant's thinking where the agent recorded it |
| `--subagent` | ID | read this subagent transcript instead of the main one; `'*'` lists the ids |

`show` also accepts a bare native id (the one the agent's own `--resume` wants) and falls back to looking it up. When the same id exists on more than one host it lists the candidates instead of guessing.

Long transcripts are paginated: when the reply ends with a `from_seq` hint, run it again with `--from <that number>`.

```bash
hindsight-cli show 'claude-code:1b2c3d4e-…'
hindsight-cli show 'hindsight://session/claude-code:1b2c3d4e-…#42' --tools
hindsight-cli show 'codex:0195c2a1-…' --from 120 --messages 40
hindsight-cli show 'claude-code:1b2c3d4e-…' --subagent agent-a1b2c3d4
hindsight-cli show 'claude-code:1b2c3d4e-…' --subagent '*'
```

### `projects`

Working directories that have session history, most recently active first, with session counts. Use it to find the right `--project` value. Options: `--since`, `--limit` (default 50, max 200).

### `memories`

The memory files agents keep for themselves — Claude Code's per-project auto-memory (`~/.claude/projects/<project>/memory/`), Codex's memories (`~/.codex/memories/`, plus its per-session summaries), ZCode's per-project memory (`~/.zcode/cli/memories/projects/<project>/memory/`) — read-only, grouped by project with *user memory* (the notes that apply to every project) last. User memory is listed under any `--project` because it applies everywhere, even one that matches no indexed project. Each entry ends with a `hindsight://memory/…` reference; pass it to `show` to read the file, which is read live from disk. Instruction files (CLAUDE.md, AGENTS.md, GEMINI.md, `.cursor/rules`, `.kiro/steering`, `copilot-instructions.md`, from the agents' homes and from every indexed project root) are listed alongside; Settings → Memory locations decides which sources are read. Options: `--project`, `--agent`, `--limit` (default 50, max 100; it caps the project memory — user memory is always included).

```bash
hindsight-cli memories --project "$PWD"
hindsight-cli show 'hindsight://memory/claude-code:/Users/me/.claude/projects/-Users-me-app/memory/MEMORY.md'
```

`search` appends up to five memory files that mention the query, with the same references; `--project`, `--agent` and `--since` apply to them too.

### `setup`

Prints this binary's path, the index path, how to put it on your `PATH`, a paste-able block that tells an agent when and how to use it, and — when `hindsight-mcp` sits next to it — the one-line MCP setup for Claude Code. It installs nothing and never edits another tool's config files. The one thing it can write is Hindsight's own: resolving the default index path creates Hindsight's data directory and migrates an index left by the old `vibex` builds (`--db` skips that).

To make this automatic instead of per-project, install the skill (see [Teaching an agent to use it](#teaching-an-agent-to-use-it)).

### `context`

Prints where your coding agents left off in the project the current directory belongs to:
up to five sessions from the last 14 days, each with its last question and the first line
of its last reply. Every agent gets its latest session in before the rest are filled in by
time, so the agent you use most cannot crowd out the others, and sessions without a single
question in them (ones another agent ran for you, or ones nobody typed into yet) are left
out. It prints nothing at all when there are none:

```powershell
cd ~\Github\my-app; hindsight-cli context
```

```text
Hindsight: recent sessions in this project, across your coding agents (5 of 9 from the last 14 days):

- Claude Code · 2h ago · "Fix the login redirect" · `claude-code:1b2c…`
  last asked: "push it"
  last reply: "Pushed to main: the redirect now keeps the original query string."
- Codex · 1d ago · "Review the auth changes" · `codex:01a0…`
  last asked: "/review --base main"
  last reply: "Two issues: the session cookie is not marked Secure, and the logout route skips CSRF…"

Read one with the hindsight_get_session tool, or run `hindsight-cli show <key>`.
```

It is what the [Claude Code plugin](#claude-code-plugin) and the [Codex plugin](#codex-plugin) run: whatever the hook
prints lands in the new session's context, where a "no sessions" reply or a list of other
projects would only be noise. A subfolder counts as its project, as with `--project
"$PWD"`. Your home folder and the filesystem root never count as the project a folder
belongs to — sessions started there are not about the project you open next. With
`HINDSIGHT_ON_REQUEST` on, it prints nothing (see [Only when you ask](mcp.md#only-when-you-ask)).


### `index`

Builds the index once, for the case where Hindsight is installed but has never been launched:

```bash
hindsight-cli index
```

It scans the agents' files and writes Hindsight's index, then tells you what it found. It only
does this when there is **no index yet** — if one already exists it says so and changes
nothing; to bring an existing index up to date use [`refresh`](#refresh). There is
deliberately no `--force`: rebuilding an index that exists is the app's job. If Hindsight is
running (and about to build the index itself), `index` steps aside the same way. In a
terminal it shows progress while it scans; when piped or run by an agent, stderr stays
quiet until the summary.

### `refresh`

Brings an existing index up to date, for the case where Hindsight is closed most of the time —
you use it through the MCP server or this CLI and only open the app to browse:

```bash
hindsight-cli refresh
```

It does the incremental pass the app does on launch — picks up new and changed sessions,
drops the deleted ones and re-reads the memory files — and then says what the index holds.
(The app's Refresh button re-reads every session; `refresh` only reads what changed.) It does not sync remote hosts; those mirrors update when the app
runs. Like `index`, it shows progress in a terminal and stays quiet when piped. Point it at
Hindsight's own database — the path `hindsight-cli setup` prints — rather than a copy or an alias: it
refuses a file that is not a Hindsight index, and a path whose remote-host mirrors are not next to it.

It steps aside while Hindsight has its window open or is still scanning: closing the window
releases the index once any scan in flight finishes, so a Hindsight process left running without a window
(for example in the macOS Dock) does not block it. If Hindsight or another writer holds the index it says so and exits `0` without
touching anything: the app is already keeping the index current, and two writers with
possibly different environments would undo each other's work. The app and the CLI share one
lock on the index for this — Hindsight holds it while its window is open, `refresh` and `index`
hold it while they work, and Hindsight waits up to a minute for a running `refresh` to finish
before it opens the index (if the index is still busy after that, or another Hindsight holds it,
Hindsight explains and exits).
The lock is three small files next to the index (`hindsight.db.lock`, `hindsight.db.lock.app`,
`hindsight.db.lock.holder`); they are safe to leave alone.

#### Keeping the index fresh without the app

Point a scheduler at the `hindsight-cli` that ships with Hindsight, so it updates together with the app.

**Windows** — a Task Scheduler task that runs every 10 minutes while you are signed in. In PowerShell (no admin rights needed; adjust the path to where you unzipped Hindsight):

```powershell
$action  = New-ScheduledTaskAction -Execute 'C:\Tools\Hindsight\hindsight-cli.exe' -Argument 'refresh'
$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 10)
$set     = New-ScheduledTaskSettingsSet -StartWhenAvailable -DontStopIfGoingOnBatteries -AllowStartIfOnBatteries
Register-ScheduledTask -TaskName 'Hindsight refresh' -Action $action -Trigger $trigger -Settings $set
```

or the one-liner equivalent:

```powershell
schtasks /Create /SC MINUTE /MO 10 /TN "Hindsight refresh" /TR "'C:\Tools\Hindsight\hindsight-cli.exe' refresh"
```

`hindsight-cli.exe` is a console program, so each run flashes a console window briefly; to avoid that, open the task in Task Scheduler (`taskschd.msc`) and choose *Run whether user is logged on or not*. `Unregister-ScheduledTask -TaskName 'Hindsight refresh'` removes it.

**macOS** — save this as `~/Library/LaunchAgents/dev.liberty0722.hindsight.refresh.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.liberty0722.hindsight.refresh</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Applications/Hindsight.app/Contents/MacOS/hindsight-cli</string>
    <string>refresh</string>
  </array>
  <key>StartInterval</key>
  <integer>600</integer>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
```

then load it once:

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.liberty0722.hindsight.refresh.plist
```

**Linux** — a systemd user timer. `~/.config/systemd/user/hindsight-refresh.service`:

```ini
[Unit]
Description=Refresh Hindsight's session index

[Service]
Type=oneshot
ExecStart=/usr/bin/hindsight-cli refresh
```

and `~/.config/systemd/user/hindsight-refresh.timer`:

```ini
[Unit]
Description=Refresh Hindsight's session index every 10 minutes

[Timer]
OnBootSec=2min
OnUnitActiveSec=10min

[Install]
WantedBy=timers.target
```

then `systemctl --user enable --now hindsight-refresh.timer`. Use `%h/.local/bin/hindsight-cli` in
`ExecStart` for a tar.gz install.

Two things to know:

- A scheduler does not see variables you set in a terminal session (`$env:X = …`). If you set
  `CODEX_HOME`, `XDG_DATA_HOME` or `HINDSIGHT_HOME` for the app, set them as user environment
  variables (`[Environment]::SetEnvironmentVariable(…, "User")` on Windows) so the scheduled job
  sees them too — an index kept by two processes with different roots would flip-flop.
- Ten minutes is a sensible interval: a refresh with nothing new takes well under a second,
  and every reply already says how fresh the index is.

## Teaching an agent to use it

Two ways to teach it, same content. To hand every new session this project's recent
sessions before the agent even asks, see [Claude Code plugin](#claude-code-plugin) and
[Codex plugin](#codex-plugin).

**A skill, once, for every project** — the repository ships one at `skills/hindsight/`:

```bash
npx skills add liberty0722/Hindsight
```

That works for Claude Code, Codex and anything else that reads the skills format. To do
it by hand, copy `skills/hindsight/` into `~/.claude/skills/hindsight/` (`%USERPROFILE%\.claude\skills\hindsight\` on Windows). The skill tells the agent
*when* to look back — earlier conversations, past decisions, where work stopped, whether
an error has been seen before — and how to read keys, references and pages.

**A paste block, for one project** — `hindsight-cli setup` prints a short version you can drop
into that project's `CLAUDE.md` or `AGENTS.md`. Use this when you would rather not
install anything, or want the guidance to live in the repository.

## Claude Code plugin

The Hindsight plugin starts every new Claude Code session knowing where your agents left off in
this project — Claude Code, Codex and every other agent Hindsight indexes, each session with its
last question and reply — so "continue from yesterday" or "pick up what Codex was doing"
needs no explaining, and it adds Hindsight's MCP tools so Claude can open any of them with
`hindsight_get_session`. Underneath it is a session-start hook that runs
[`hindsight-cli context`](#context), which prints nothing in a project without recent history.
It needs Hindsight 0.1.0 or later and covers macOS and Linux. Settings → Connect shows the
command with a copy button.

```bash
claude plugin marketplace add liberty0722/Hindsight --sparse .claude-plugin plugins/hindsight && claude plugin install hindsight@hindsight
```

`--sparse` checks out only the plugin's folders instead of the whole repository. Inside a
session, `/plugin marketplace add liberty0722/Hindsight` and then `/plugin install hindsight@hindsight` do the
same. The plugin carries no binaries of its own: it runs the `hindsight-cli` and `hindsight-mcp`
inside your installed Hindsight (set `HINDSIGHT_BIN_DIR` to their folder if Hindsight lives somewhere
other than the usual place). If you added the `hindsight` MCP server by hand earlier, remove it
with `claude mcp remove hindsight` so the tools are not listed twice. `claude plugin update
hindsight@hindsight` updates it, or turn on auto-update for the `hindsight` marketplace in `/plugin`.

**Without the plugin**, the same hook goes in `~/.claude/settings.json` (every project) or
in a project's `.claude/settings.json`; add the MCP server as [docs/mcp.md](mcp.md#claude-code)
shows:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|clear",
        "hooks": [
          {
            "type": "command",
            "command": "hindsight-cli context 2>/dev/null || true",
            "timeout": 30
          }
        ]
      }
    ]
  }
}
```

Use `hindsight-cli`'s full path if it is not on your `PATH` (see *Where it lives*).

## Codex plugin

The same plugin works in Codex, in the CLI and in the Codex app: every new session starts
knowing where your agents left off in the project, and Codex gets Hindsight's MCP tools to read
those sessions. It needs Hindsight 0.1.0 or later and covers macOS and Linux. Settings → Connect
shows the command with a copy button.

```bash
codex plugin marketplace add liberty0722/Hindsight --sparse .agents/plugins --sparse plugins/hindsight && codex plugin add hindsight@hindsight
```

Codex runs a plugin's hook only after you trust it: start Codex, open `/hooks` and trust
Hindsight's session-start hook. Codex asks again whenever the hook changes. As with Claude Code,
the plugin carries no binaries and runs the `hindsight-cli` and `hindsight-mcp` inside your installed
Hindsight. If you added the `hindsight` MCP server to `~/.codex/config.toml` by hand earlier, remove
that entry so the tools are not listed twice; likewise, if you pasted Hindsight's
`[[hooks.SessionStart]]` block there from Hindsight 0.1.0's Settings → Connect, remove it, or
every new session gets the context twice.

**Without the plugin**, add the hook to `~/.codex/config.toml` yourself, approve it in
`/hooks`, and add the MCP server as [docs/mcp.md](mcp.md#codex) shows:

```toml
[[hooks.SessionStart]]
matcher = "startup|clear"

[[hooks.SessionStart.hooks]]
type = "command"
command = "/Applications/Hindsight.app/Contents/MacOS/hindsight-cli context 2>/dev/null || true"
timeout = 30
```

For both:

- What gets injected is up to five recent sessions, each with its agent, title, key, last
  question and the first line of its last reply — not transcripts. The agent reads one with
  `hindsight_get_session` or `hindsight-cli show` when it needs the details.
- `startup|clear` covers new sessions and `/clear`. A resumed or compacted session keeps
  its own history, so the hook stays out of those.
- A lookup made by the hook is not the agent asking Hindsight, so it does not show up in
  Insights under *Agents asking Hindsight*; the lookups the agent then makes on its own do.
- To keep Hindsight quiet until you ask, set `HINDSIGHT_ON_REQUEST=1` in the environment Claude Code
  or Codex starts from: the session-start hook adds nothing, and the agent uses Hindsight's
  tools only when you ask (see [Only when you ask](mcp.md#only-when-you-ask)).
- None of this edits another tool's configuration: Claude Code and Codex install the plugin
  themselves, and the hook settings are yours to paste.
- On Windows, use the skill and the MCP server for now.

## When

`--since` accepts a relative window — `30m`, `24h`, `7d`, `2w` — or an absolute date/time: `2026-09-01`, `"2026-09-01 09:30"`, `2026-09-01T09:30:00Z`. Date-times without a zone are read in local time.

Numeric options outside their range are clamped, not rejected: `--limit 999` on `sessions` gives you 100.

## Agent ids

`claude-code`, `codex`, `grok`, `grok-bot`, `dsh`, `cursor`, `opencode`, `pi`, `omp`, `kiro`, `kimi`, `gemini`, `copilot`, `antigravity`, `qoder`, `hermes`, `openclaw`, `codebuddy`, `workbuddy`, `zcode`, `craft-agents`, `devin`, `kilo`, `mimo-code`, `with`. Display names (`"Claude Code"`) and a few aliases (`claude`, `deepseek`, `craft`, `mimo`) work too.

## Session keys and references

- Local sessions: `<agent>:<native id>`, for example `codex:0195c2a1-…`.
- Sessions mirrored from a remote host: `<agent>:<host>:<native id>`.
- References: `hindsight://session/<key>#<seq>` point at one message; `show` accepts them and starts the page there.

The native id is the one the agent's own `--resume` flag expects.

## Output

The text is Markdown, written for an agent to read, and identical to what the MCP tools return. One consequence is visible: the closing hint in a listing names the MCP tool (`hindsight_get_session`) rather than `hindsight-cli show`, because the same string serves both surfaces. Read it as "the command that reads a session".

`search`, `sessions` and `projects` normally end with a line saying how fresh the index is (`Index covers activity up to …`) — that is the newest activity Hindsight has indexed, so a stale-looking time usually means Hindsight has not been running, though agents stored in SQLite (Copilot, OpenCode, Antigravity, Hermes, OpenClaw) only refresh on launch or Refresh either way. Two replies skip the line: an unrecognised `--project`, which returns early with the list of known projects, and an empty index, which says so instead. `show` never has one because it reads the agent's file rather than the index. For a session mirrored from a remote host that file is the local mirror, current as of the last successful sync — Settings → Remote hosts shows whether the last one succeeded.

Exactly one trailing newline is added when the text does not already end with one — nothing else is added or removed. Piping is safe: `hindsight-cli show <key> --tools | head -3` prints three lines and exits 0.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | it ran — **including** "no matches", "no such project" and "no sessions" |
| `1` | it ran and failed — unknown or ambiguous key, a transcript that would not parse, a query that errored, or a failed write to stdout |
| `2` | the command line was wrong, or the index is missing / unreadable / too old |

Empty results are never an error, so `hindsight-cli search x || fallback` does not fire just because nothing was ever discussed about `x`. Diagnostics go to stderr, prefixed `hindsight-cli: `, so `hindsight-cli show <key> > out.md` cannot capture one. Two commands treat a missing index as normal rather than as the `2` above. `setup` always exits `0`, even with no index, and reports a missing one as a `Note:` line on stdout — "not set up yet" is the normal state for someone running it. `index` exits `0` both when it builds one and when it declines because one already exists or because Hindsight or another writer holds the index; a build that starts and then fails exits `1`. `context` exits `0` and prints nothing when the folder has no recent sessions or belongs to no indexed project. `refresh` exits `0` when it updates the index and when it declines because Hindsight or another writer holds the index; it exits `2` when there is no index, when `--db` is not a Hindsight index, or when the index holds remote-host sessions or memories whose mirrors are not next to that path (a copy or an alias in another directory); a scan that starts and then fails exits `1`.

Both a mistyped option (`--sinse 7d`) and a value the tools reject (`--since 7dd`) exit `2` — from a user's seat they are the same mistake, and the layer that caught it is not visible. This is a deliberate difference from `hindsight-mcp call`, which exits `1` for anything the tool layer rejects — its JSON-RPC envelope already carries the classification, so the exit code never had to. (`hindsight-mcp` still uses `2` for its own argv problems: a missing tool name, unparsable JSON, or an index it cannot open.)

## Privacy and scope

- Everything runs on this machine. The CLI makes no network requests.
- It reads the same session files Hindsight indexes — local agents' data directories plus the local mirrors of any remote hosts you configured in Hindsight. Nothing leaves the machine.
- Hindsight's read-only rules apply: other agents' directories and databases are opened read-only and credential files are never read. Every command except `index` and `refresh` opens Hindsight's own index without write access too, and a test asserts that the query commands never change an index by a byte — nor does `index` when one already exists, nor `refresh` while Hindsight is running.
- On its first run against the default database path, Hindsight's shared path helper migrates an index left by the old `vibex` builds. `hindsight-mcp` does the same; `--db` skips it entirely.

## Troubleshooting

- **`no Hindsight index at … — launch Hindsight once to build it`.** Hindsight has never run on this machine, or `--db` points at the wrong file. If Hindsight is installed but has never been launched, `hindsight-cli index` builds the index from a terminal instead. `… is empty or from an older version` means the index predates this Hindsight version; launching Hindsight once upgrades it, and so does `hindsight-cli refresh`.
- **`No indexed project matches …`.** Pass the absolute path of the repository, or its name. `hindsight-cli projects` shows the paths Hindsight knows.
- **Results look stale.** Keep Hindsight running, or schedule [`hindsight-cli refresh`](#refresh) for the times it is closed; the freshness line at the end of every reply says what the index covers. Copilot / OpenCode / Antigravity / Hermes / OpenClaw databases refresh when Hindsight launches, when you click Refresh, or on `hindsight-cli refresh`.
- **Windows: `hindsight-cli` is not recognized.** The folder is not on `PATH`, or the terminal was opened before you added it — open a new one (see [Where it lives](#where-it-lives)), or call it by full path.
- **Windows blocks it ("Windows protected your PC" / "This app has been blocked").** The binaries are not code-signed. Run `Hindsight.exe` once and choose *More info* → *Run anyway*, or clear the download mark for the whole folder: `Get-ChildItem C:\Tools\Hindsight | Unblock-File`.
- **macOS refuses to run it ("cannot be opened because the developer cannot be verified").** Hindsight is signed but not notarized. Clear the quarantine flag for the whole bundle once: `xattr -dr com.apple.quarantine /Applications/Hindsight.app`.

## See also

[docs/mcp.md](mcp.md) — the same index over MCP, for clients that call tools rather than run commands.
