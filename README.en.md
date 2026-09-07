# OFFICESTRA

[한국어](README.md) | **English**

> Put Claude Code, Codex, and Antigravity in the same office.

**This is not another model picker. It is a control room for independent AI sessions.**

OFFICESTRA is a local-first multi-AI orchestration app for macOS that turns
Claude Code, Codex, and Antigravity coding agents into a visible team. Every
coworker keeps its own provider, model, role, and conversation session. Instead
of jumping between terminals and chat windows, you can divide the work, watch it
unfold, and bring in the next AI from one place.

App interface languages: **English · Korean (한국어)**

<p align="center">
  <img src="docs/images/officestra-social-preview.png" alt="OFFICESTRA orchestrating Claude Code, Codex, and Antigravity in one place" width="100%">
</p>

<p align="center">
  <strong>Three AI providers · Five independent sessions · One local control room</strong>
</p>

## Why should using more AI make work more complicated?

Claude Code conversations stay in Claude Code. Codex work stays in Codex.
Antigravity sessions stay in Antigravity. OFFICESTRA does not force them into
one artificial conversation. It preserves each provider's native flow and
brings their operation into one shared view.

## The features that matter

### Five AI coworkers, five independent sessions

Assign Claude Code, Codex, or Antigravity to each coworker, then give every one
a different role and model. Their conversations never get mixed together, and
each coworker resumes its own session when you return. However, all conversation
history is shared through the database, RAG, and the internal wiki.

### New models and reasoning options, automatically — you choose

OFFICESTRA automatically refreshes the available models and supported reasoning
levels from Claude Code, Codex, and Antigravity. New models appear in the picker
automatically. In settings, hide only the models you do not want to see, then
choose each coworker's model and reasoning level yourself.

### Run work in parallel and see it all

Give different tasks to several coworkers at once. For each coworker, see who is
thinking, which commands and tools are running, what changed, and what answer
they produced.

### Let a lead AI delegate and consolidate

Ask the lead coworker to assign work to several coworkers. Their conversations
remain independent while the lead coworker gathers the results and finishes the
job.

### Move between the GUI and a real terminal

Open a coworker's current session as an interactive native CLI whenever you
want direct control. Return to the GUI and the terminal work remains part of
that coworker's history.

### Keep conversations, work, and limits together

Completed work and conversations stay local. Find a coworker's previous work,
check each CLI's remaining limits and context, and decide which AI should take
the next task.

## Two ways to work: GUI and real terminal

<table>
  <tr>
    <td width="50%" align="center">
      <img src="docs/images/officestra-gui-mode.png" alt="OFFICESTRA GUI mode" width="100%"><br>
      <strong>GUI mode</strong><br>
      <sub>Conversations, work history, and coworkers in one view</sub>
    </td>
    <td width="50%" align="center">
      <img src="docs/images/officestra-terminal-mode.png" alt="Claude Code CLI running inside OFFICESTRA terminal mode" width="100%"><br>
      <strong>Terminal mode</strong><br>
      <sub>The same coworker's live Claude Code CLI session</sub>
    </td>
  </tr>
</table>

Move between a conversational GUI and real interactive CLIs while keeping each
coworker's progress, limits, and history in one place. Visual styles can change;
the underlying AI sessions and workflow stay intact.

## OFFICESTRA is for you if

- You use Claude Code, Codex, and Antigravity but are tired of managing windows and sessions.
- You prefer separate role-based AI conversations over one enormous chat.
- You want to explore multi-AI orchestration without building a platform yourself.
- You want one clear view of what every AI is actually doing.

## Supported CLIs

- OpenAI Codex CLI
- Anthropic Claude Code
- Google Antigravity CLI

Available models, reasoning levels, and limit information depend on each
account and installed CLI version.

## Get started

### Easiest path: ask an AI to do it

Send the following request to Codex, Claude Code, or Antigravity if you already
use one of them:

> “Download `https://github.com/neosp8888-design/officestra.git` to this Mac and run
> OFFICESTRA. Preserve my existing AI CLI logins and projects, inspect the
> environment first, and install only missing dependencies. Verify that both the
> app and its local backend are running.”

### Download the app

[Download OFFICESTRA v1.4.0 DMG](https://github.com/neosp8888-design/officestra/releases/tag/v1.4.0)
— Apple silicon · macOS 14 or later · English/Korean.

Open the DMG and drag OFFICESTRA into Applications. Node.js is included. You still
need Docker Desktop and the AI CLIs you want to use, installed and signed in.

> This is a **Community Preview** without Apple notarization. If macOS blocks
> launch, verify the download's source, then use **Open Anyway** for this app in
> System Settings → Privacy & Security. Preserve your existing conversations and
> work records when updating.

<details>
<summary><strong>Manual installation — for experienced users</strong></summary>

### 1. Requirements

- An Apple silicon Mac running macOS 14 or later
- Xcode Command Line Tools with Git and Swift 5.10 or later
- Node.js 20 or later and npm
- Docker Desktop
- At least one signed-in Codex, Claude Code, or Antigravity CLI

Install Apple's command-line developer tools.

```sh
xcode-select --install
```

If [Homebrew](https://brew.sh/) is available, install Node.js and Docker Desktop,
then launch Docker.

```sh
brew install node
brew install --cask docker
open -a Docker
```

### 2. Install and sign in to an AI CLI

You do not need all three. Install only the CLIs you plan to use, then launch
each one once to sign in to its service. Run only the command blocks you need.

**OpenAI Codex CLI** — [official guide](https://developers.openai.com/codex/cli/)

```sh
curl -fsSL https://chatgpt.com/codex/install.sh | sh
codex
```

**Anthropic Claude Code** — [official guide](https://code.claude.com/docs/en/getting-started)

```sh
curl -fsSL https://claude.ai/install.sh | bash
claude
```

**Google Antigravity CLI** — [official guide](https://codelabs.developers.google.com/antigravity-cli-hands-on)

```sh
curl -fsSL https://antigravity.google/cli/install.sh | bash
agy
```

If Docker installation fails, see the
[official Docker Desktop guide](https://docs.docker.com/desktop/setup/install/mac-install/).

### 3. Download the repository

```sh
git clone https://github.com/neosp8888-design/officestra.git "$HOME/OFFICESTRA"
cd "$HOME/OFFICESTRA"
```

If that location already contains an installation, inspect it first instead of
overwriting or deleting it.

### 4. Start the backend and app

Start the local database and backend in the first Terminal window.

```sh
cd "$HOME/OFFICESTRA"
./scripts/start-backend.sh
```

The first run may take a while while Docker images and packages download. When
it is ready, start the app from a second Terminal window.

```sh
cd "$HOME/OFFICESTRA"
swift run OfficeLLM
```

When the app opens, use the first-run assistant to select a workspace and assign
your signed-in CLIs to coworkers.

</details>

> [!WARNING]
> OFFICESTRA is still a preview. Back up important work. Check conversations and
> work records for sensitive information before sharing them.

[License](LICENSE)
