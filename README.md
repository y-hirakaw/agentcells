# agentcells

English | [日本語](docs/README.ja.md)

Create an Issue, and cells of AI agents move the work forward. A dispatcher decides whether work goes to the next step or to a human, and questions and requests for human review reach people through Issues and PRs. Think of it as graph engineering, or a software factory.

> **This is an experimental project and a work in progress.** The specification may change without notice. The concept is in [concept.md](concept.md), the specification of what has been built is in [docs/design.md](docs/design.md), and the change history is in [CHANGELOG.md](CHANGELOG.md) (all in Japanese).

## Goals

- Once AI agents are set up as configured, creating an Issue on GitHub is all it takes to move the work forward
- Review comments and questions happen on Issues and PRs, just like working with people
  - "There is something I want to confirm" (a question)
  - "The PR is ready, please check it" (a request for human review)
- Each cell runs as an independent loop, and cells can be combined

## This repository is developed with agentcells too

Create an Issue in this repository, and a PR is opened, reviewed by AI, and updated to address the review comments. Depending on the change, it is merged automatically or a human is asked to review it.

## Structure (planned)

| Role | What it does |
| --- | --- |
| cell | An independent loop that runs one kind of work, such as triage, design, implementation, or review |
| dispatcher | Decides which cell gets the work next, or whether it goes to a human |
| inspector | Watches whether cells are running and where the yield drops, and turns findings into Issues |
| maintenance | Takes the Issues the inspector reports and fixes the production line itself |
| shipping | Ships (releases) the output. Its responsibility is kept separate from manufacturing |

## Install (work in progress)

For now, build from source. If Rust is not installed, install it first by following https://rustup.rs.

```sh
git clone https://github.com/y-hirakaw/agentcells.git
cd agentcells
./install.sh
```

This installs `~/.agentcells/bin/agentcells` and then runs `agentcells setup`. If anything is missing, it shows how to fix it. You can change the location with the `AGENTCELLS_HOME` environment variable.

To update, run the following in the cloned repository. Until you reinstall, your `agentcells` stays at the old version (check with `agentcells --version`). When a new version adds settings, [CHANGELOG.md](CHANGELOG.md) says so.

```sh
git pull
./install.sh
agentcells doctor
```

If new settings or labels are needed, `agentcells doctor` shows how to add them.

| Command | What it does |
| --- | --- |
| `agentcells doctor` | Checks that everything needed is in place (tools, logins, settings, GitHub App, labels) and shows how to fix what is not |
| `agentcells setup` | Prepares the data directory and a template config file (`config.toml`). Safe to run any number of times |
| `agentcells labels sync [--dry-run]` | Creates the `ac:` labels that the products and cells in your config need but GitHub does not have yet |
| `agentcells files apply <owner/repo> <worktree>` | Places gitignored files at the same paths in a worktree |
| `agentcells tick [--dry-run]` | Finds work (Issues/PRs you chose that carry a cell label, and items waiting on a human that you replied to) and runs a cell on each, one at a time. While work remains, it keeps going up to a limit (`--max-minutes`, 30 minutes by default). It also removes `ac:` labels left on closed Issues/PRs. `--dry-run` only shows what it would do. implement checks the commits, pushes, and opens a PR. review posts a review on the PR |
| `agentcells schedule install [--every <minutes>]` / `remove` | Registers (or removes) a launchd job (macOS) that runs `tick` at a fixed interval (10 minutes by default). While a cell is running, the next tick does nothing. The log is `~/.agentcells/logs/tick.log` |
| `agentcells inspect [--dry-run]` | Checks the tick records (`~/.agentcells/logs/events.jsonl`) with fixed rules to find problems in the line (the same cell running twice in a row, AI runs failing, and so on), and has AI write the cause and a proposed fix from the agentcells source. Opens Issues in the repository set in `[inspector] issues` (or only keeps them locally if unset). With `[inspector]` set, tick also runs this at the end once a day. `--dry-run` only prints the masked records that would be given to the AI |
| `agentcells status [--watch]` | Shows the running cell (elapsed time, the tool the AI is using) and, for each Issue, how far it has progressed (working, waiting for a reply, waiting for human review, queued) and since when. For Issues with a PR, shows the PR's state (read-only). `--watch` refreshes only the running cell every 2 seconds |
| `agentcells prompt show <cell> [--issue owner/repo#N \| --pr owner/repo#N]` | Builds and prints the prompt given to the AI running as a cell (the standard part, your additional instructions, and the current task) |

In the config file `~/.agentcells/config.toml`, write your GitHub login as the person who uses the cells (`operator`), the products (which repository holds the Issues and which repositories the work happens in), and the cells (which label each picks up and which prompt it runs with). See [docs/design.md](docs/design.md) and the comments in the template for the format. Cells run only on Issues/PRs where you have added yourself to Assignees and added a cell label (changes to labels or Assignees by other people do not start anything). When the AI stops to ask a question (`ac:needs-human`) or a PR is waiting for human review (`ac:in-review`), just comment as you would reply to a person, and the work continues (other people's comments do not trigger it; every comment of yours counts as a reply). The standard cells (triage, implement, review, assess) run with the standard prompts in [prompts/](prompts/) without you writing a prompt. If you set `prompt` to a file, it is added as additional instructions.

### GitHub App (recommended)

Comments and PRs are written by a GitHub App, or by the account you are logged in to with gh (only github.com is supported for now). With your own account, the AI's comments look like yours and are hard to tell apart, so creating a GitHub App is recommended.

1. Create one from GitHub Settings → Developer settings → GitHub Apps → New GitHub App
   - Webhooks are not used, so uncheck Active
   - Repository permissions: set Contents, Issues, and Pull requests to Read and write (Workflows is not needed: agentcells does not push commits that change `.github/workflows`)
   - "Where can this GitHub App be installed?" can be "Only on this account"
2. On the app's page, click Generate a private key, put the downloaded `.pem` at `~/.agentcells/github-app.pem`, and run `chmod 600` on it
3. From Install App, install it on the Issue repository and the repositories where the work happens
4. Add it to `~/.agentcells/config.toml`

   ```toml
   [github_app]
   id = "Iv23li..."                 # the Client ID on the app's page
   private_key = "github-app.pem"
   ```

5. Check with `agentcells doctor`. If the app is missing from a repository, it shows the installation URL

## Development

The Rust version is pinned in `rust-toolchain.toml`.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Supported agents

It is built for Claude Code first. Names and design avoid depending on a specific agent.

## License

MIT ([LICENSE](LICENSE))
