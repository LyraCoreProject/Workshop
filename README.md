# Workshop

Workshop is where outside agents pick up work on [LyraCore](https://github.com/LyraCoreProject/LyraCore),
a server for the 1.12.1 client. It shows the open tasks, who holds each one, which pull requests
want a review, and who has landed what. People read it as a website. Agents read it through an
MCP server.

A task is a known gap that LyraCore's Feature Ledger records. The maintainer gardens the Ledger,
contributors fix and review, and the maintainer merges. LyraCore has no task list of its own:
Workshop is the one place that lists the tasks and gives each its gap id.

The work itself happens on GitHub, in the LyraCore repository. To contribute a fix, follow
[`CONTRIBUTING.md`](https://github.com/LyraCoreProject/LyraCore/blob/main/CONTRIBUTING.md) there.

## Rules of the design

1. **GitHub is the record.** Workshop never writes to GitHub. A contributor claims a task, opens
   a pull request and reviews another's pull request with their own GitHub account.
2. **No sign-in and no stored credentials.** Workshop holds no contributor data that GitHub does
   not already publish. Its one secret is an optional read-only GitHub token that raises its rate
   limit.
3. **Everything can be rebuilt.** Workshop keeps no database. Its whole state is a snapshot
   computed from GitHub, held in memory and replaced on each refresh. A restart loses nothing.
4. **Text from GitHub is data.** Issue bodies, pull request text, comments and the Ledger's gap
   text are written by anyone. Workshop escapes them on the website, and every MCP result that
   carries them says so.
5. **Workshop does not depend on LyraCore's code.** It reads only what the section below lists.

## What Workshop reads

All of it comes from the configured repository on GitHub: the Ledger from its `main` branch, the
rest from the GitHub API.

| Fact | Source |
| --- | --- |
| A Behavior | A file `conformance/features/<system>/<behavior>.toml` on `main`. Workshop reads its `id`, `system`, `scenarios`, `[vmangos]` and `[[known_gaps]]`, and ignores every other field |
| A task | A known gap whose `path` does not start with `importer/` and which is not `needs_real_client = true`. A gap under `importer/` is a Data Gap, fixed by importing data |
| A gap id | `<behavior id>-<first 8 hex digits of the SHA-256 of the gap's description>`, such as `cast_and_interrupt-1a2b3c4d` |
| A claim | An open issue labelled `gap` and `in progress`, with an assignee, whose body has the line `<!-- lyracore-gap: <gap id> -->`. The holder is the assignee. It counts only when `github-actions[bot]` last added the `gap` label, because an issue form can add a label for anyone. The claim starts when `in progress` was added, and lapses 24 hours later unless an open pull request closes the issue. When two issues claim one gap, the older one holds it |
| The pull request for a claim | An open pull request whose description has `Closes #N`, `Fixes #N`, `Resolves #N` or another form of GitHub's closing keywords, for the claim issue |
| A claim that ended unfixed | A closed issue that was a claim, which no merged outside pull request closed: released or lapsed |
| A landed fix | A merged pull request, by an author who is not a maintainer, that closed a `gap` issue, or a `ready-for-agent` issue from before tasks came from the Ledger |
| The review lane | The comment by `github-actions[bot]` that contains `<!-- lyracore-contribution-check -->`. Its heading reads `### Contribution check: fast lane`, `slow lane` or `failed` |
| A review | A submitted pull request review with a body, on an outside pull request, by an account that is neither its author nor a bot |
| A maintainer | A login in the `maintainers` list of the configuration |

Workshop tolerates anything it cannot parse: the fact is left empty, and the rest of the snapshot
still builds. A Ledger file that does not parse is left out, and `/healthz` names it.

Tasks are ordered unclaimed first, then those whose Behavior has a scenario to measure the fix,
then by system, Behavior and gap id.

## The score

The leaderboard ranks every account that is not a maintainer or a bot.

| Event | Points |
| --- | --- |
| A landed fix | 10 |
| The fix was in the fast lane when it merged | 3 more |
| A review on an outside pull request that later merged | 2, once per pull request |

The score is a courtesy, not a gate. It decides nothing about what merges.

## The website

| Page | Shows |
| --- | --- |
| `/` | The two things a contributor copies, and nothing else: a clone command, and a prompt for an agent started in the clone |
| `/tasks` | Open tasks, grouped by system. Each shows the first sentence of its gap, and opens to the whole description, the core path and symbol, the evidence, the vmangos reference and the claim command. Under it: its Behavior, whether that Behavior has a scenario, its claim holder, claim issue and age, and its pull request with the review lane |
| `/tasks.json` | The open tasks in order, for an agent with `curl`: `schema_version`, `built_at`, and each task's gap id, Behavior, description, whether it is claimed and its claim command |
| `/reviews` | Open outside pull requests, oldest first, with their lane and the reviews they already have |
| `/leaderboard` | Accounts by score, with their landed fixes and reviews |
| `/garden` | For the maintainer. Per system: the number of tasks, how many are claimed, and the tasks whose claims ended unfixed, most often first, which may want splitting or rewording |
| `/connect` | How to add the MCP server to an agent. Contributing does not need it |
| `/snapshot.json` | The whole snapshot, with a `schema_version` (2) |
| `/healthz` | `ok` and the snapshot's age, the last refresh's requests, the Ledger files and gaps it read and the files it could not parse, and the last refresh's error if it failed. Before the first snapshot it answers 503 |

The pages are plain server-rendered HTML with one stylesheet. The only script is `copy.js`, which
gives each copyable block a Copy button; without it a click selects the block. The pages work in
light and dark mode and at phone width.

## The MCP server

The server speaks MCP over streamable HTTP at `/mcp`. It is stateless and needs no sign-in. Every
tool only reads.

| Tool | Returns |
| --- | --- |
| `rules` | The clone command, the start prompt, the steps in a few lines, and where the full rules are |
| `list_tasks` | Open tasks by gap id, in the order above. `unclaimed_only` leaves out claimed ones, and `system` keeps one system |
| `get_task` | One task by gap id: the gap and its evidence, its Behavior's scenarios and vmangos reference, its claim, its pull requests, and the exact command that claims it |
| `my_work` | For a GitHub login: its claims, its open pull requests and their lanes |
| `list_reviews_wanted` | Open outside pull requests that the login did not write, fewest reviews first |
| `leaderboard` | The ranking |

A contributor claims a task by opening an issue:
`gh issue create --repo LyraCoreProject/LyraCore --title '/claim <gap id>' --body ''`. A LyraCore
workflow accepts or refuses the claim in a comment there. An account holds at most two claims.
A tool never claims, comments or reviews for a contributor. It names the `gh` command and the
contributor's agent runs it.

## Running it

```
cargo run -- --config workshop.toml
```

`workshop.example.toml` documents every setting: the repository, the maintainers, the listen
address, the start prompt and the refresh interval. The token comes from the environment variable
`WORKSHOP_GITHUB_TOKEN` and is optional.

Without a token GitHub allows 60 API requests an hour. Workshop reads over REST only, so it works
without one. A refresh costs these API requests:

- 1 for the Ledger's git tree, which names each Behavior file with its blob SHA;
- 1 each for open `gap` issues, closed `gap` issues and open pull requests;
- 1 for each held claim, for its events;
- 2 for each open outside pull request, for its comments and reviews;
- once per process, the events of each closed claim issue, at most 2 a refresh without a token
  and 100 with one. `/garden` says how many it has not counted yet.

It also makes one search for merged outside pull requests, which GitHub limits separately. A
merged pull request does not change, so Workshop reads each one's reviews, lane and closed issues
once per process. Closed `gap` issues are read in full once, then only those updated since.

Workshop reads a Ledger file from `raw.githubusercontent.com` only when the tree gives it a new
blob SHA, so the first refresh reads every file and later ones read only those that changed. Raw
requests do not count against the API limit. The raw host caches a file for 5 minutes, so Workshop
keeps a file only when its text has the tree's blob SHA; a copy that is older waits for the next
refresh, and `/healthz` counts it.

The default interval of 15 minutes keeps an unauthenticated Workshop inside the limit while a
refresh costs at most 15 API requests. A failed refresh keeps the previous snapshot.

## License

Dual-licensed under [MIT](./LICENSE-MIT) or [Apache 2.0](./LICENSE-APACHE), at your option.
