# OFFICESTRA

[한국어](README.md) | **English**

> **Independent AI sessions. One team.**

Run Claude Code, Codex, and Antigravity together in one macOS app.<br>
**Five independent sessions · Coworker collaboration · English / Korean**

<p align="center">
  <img src="docs/images/officestra-social-preview.png" alt="OFFICESTRA bringing Claude Code, Codex, and Antigravity into one office" width="100%">
</p>

## GUI or real terminal — your choice

<table>
  <tr>
    <td width="50%" align="center">
      <a href="docs/images/officestra-gui-mode.png"><img src="docs/images/officestra-gui-mode.png" alt="Actual GUI showing coworkers, conversations, work history, and usage limits" width="100%"></a><br>
      <strong>GUI mode</strong><br>
      <sub>See conversations and work in progress</sub>
    </td>
    <td width="50%" align="center">
      <a href="docs/images/officestra-terminal-mode.png"><img src="docs/images/officestra-terminal-mode.png" alt="Actual Claude Code CLI running inside OFFICESTRA terminal mode" width="100%"></a><br>
      <strong>Terminal mode</strong><br>
      <sub>Work directly in a coworker's CLI</sub>
    </td>
  </tr>
</table>

Continue a coworker's session in the terminal, then return to the GUI to review the work.<br>
*Original app screenshots. Click to view full size. Screenshots show the Korean interface; the app also supports English.*

## Work independently. Collaborate as a team.

Split work across coworkers. A lead AI can delegate and consolidate the results,
while coworkers exchange requests, replies, and reviews through the API.
**Each conversation session stays independent.**

<p align="center">
  <a href="docs/images/officestra-architecture-en.svg"><img src="docs/images/officestra-architecture-en.svg" alt="CLI and model choices feed five independent coworkers, connected by API collaboration and shared local records" width="100%"></a>
</p>

Conversations and work records stay local. Search past work when needed,
and publish knowledge to the company wiki with your approval.

## Give coworkers instructions for working together

Use **Settings** below a coworker's conversation to customize their name and
**work instructions**. Give each coworker a distinct specialty and file scope,
while keeping the collaboration rules consistent. This example fits within the
1,200-character limit and can be pasted as-is or adapted to the role.

```text
Before starting, confirm the goal, scope, and prohibited actions.
Delegate an independent task when another coworker is better suited to it by sending one request at a time with POST /api/agent-jobs. Set characterId to the recipient and senderCharacterId to your own ID. Include the goal, owned files, prohibited actions, and the characterId that should receive the reply.
Do not stop at acknowledging a request from another coworker. Send the result, verification, and remaining risks back to the original sender in a new turn. Do not edit the same files concurrently or retry a 409 (busy) response indefinitely.
The final owner must verify coworkers' findings against source and tests, then present one consolidated conclusion.
Only delete, deploy, restart services, or expand permissions when the user has authorized it.
```

Use the **split button beside LIVE** to view two coworkers side by side in chat
or terminal mode. Choose the right coworker on first use; later splits restore
the last right coworker. Click a pane's content to select that coworker and
target the shared composer, then use the bottom coworker selector to change coworkers. Drafts and
attachments stay with each coworker; hovering and scrolling do not change the
input target. A thin gradient line with a moving highlight along the bottom identifies
the focused pane; Reduce Motion uses a static gradient. While a
new conversation loads, a loading indicator is shown and further coworker
switches are temporarily blocked. Drag the inner divider to
adjust widths. Merging keeps the current input target's conversation and leaves
running work intact. The outer office/conversation width stays under your control.

Type `@` and choose a teammate, or drag an office character or teammate selector
into the composer. Multiple recipients are saved per employee and remain selected
after sending, switching views, or restarting. Manage recipients and pause forwarding
from **Auto-forward** in the conversation header. Pausing holds pending deliveries;
replies already started continue. The app forwards final replies with the correct
sender, without model API calls. Replies to other employees use the responding
employee's own saved recipients, so reciprocal selections continue the conversation.
GUI, terminal, and queued turns use the same settings. No selection means no forwarding.
Pending deliveries wait while the recipient is busy and can be cancelled without
stopping either employee. Failed, interrupted, or confirmation-required replies
are not sent. Unsent pending deliveries survive backend restarts; an uncertain
receipt is shown for review rather than automatically replayed.

At launch, OFFICESTRA automatically adds the exact `senderCharacterId` guidance
for that coworker. It is display attribution for the message bubble, not proof
of authority. A delegated request must also name the **coworker ID that should
receive the reply**. Prefer one result-bearing reply over an endless exchange of
acknowledgements.

Check `canReceive` at `GET /api/agent-jobs?characterId=RECIPIENT_ID` for the
recipient's current availability. A work record's `lifecycleState: active` means
the record is retained; an active session means the conversation is open. Neither
means a turn is running. Running, pending, preparing, and compacting work still
blocks another execution. Preserve completed records. The sender is the current
employee and the recipient is the other employee. Never use DELETE to resolve a
409: `DELETE /api/agent-jobs/{characterId}` interrupts real work, rather than
cleaning up records. This distinction is also included in employee launch guidance.

## New models arrive automatically. You choose.

<p align="center">
  <a href="docs/images/officestra-gui-mode.png"><img src="docs/images/officestra-controls-detail.png" alt="Detail from the original screenshot showing five coworker tabs and CLI, model, reasoning, and permission controls" width="100%"></a>
</p>

Choose a different CLI, model, reasoning level, and role for each coworker.
Supported models and reasoning options refresh automatically.
In **model visibility settings**, hide only the models you do not want.<br>
*A detail cropped from the original GUI screenshot, without rescaling its pixels.*

## Local models are experimental

Local AI is not a generally auto-detected model option. It is an **opt-in,
experimental feature** that appears only after an operator registers and enables
a validated profile. Cloud CLIs remain the default, and every other OFFICESTRA
feature works without a local model.

### If you do not have a local model

- Install and sign in to only the Codex, Claude Code, or Antigravity CLIs you use.
- Choose a CLI and cloud model from each coworker's quick settings bar.
- Do not create a local profile. When no enabled profile exists, the **Local AI**
  menu stays hidden and no additional configuration is required.

### If you have a prepared local model host

- The direct path validated in this public preview is narrowly pinned to a
  **Codex runner, Responses bridge, SSH host-key-pinned Windows NVIDIA PC, and
  Qwen3.8-27B with a 64K context (q8 KV)**. This is not yet a generic form for
  arbitrary OpenAI-compatible URLs.
- The local PC needs the model files and runtime pinned by the repository. The
  Mac needs a non-interactive SSH key and pinned host key for that PC. A change
  to the model, runtime, paths, or GPU configuration requires revalidation of
  the profile and safety limits.
- Once an operator registers and enables a valid profile in the local backend,
  **Local AI** appears in the coworker's CLI menu. Finish active work and close
  the terminal before selecting it.
- Switching preserves conversation history but starts a new session. Use the
  coworker's **Settings** to verify and save the local PC's IPv4 address. Choose
  **Return to previous cloud settings** to restore the earlier CLI, model, and
  permission.

OFFICESTRA manages only local processes that it started and refuses to take over
an existing model server or a GPU running another workload. Local turns show no
API cost, but electricity and hardware costs are not tracked. This release does
not include a general one-click profile creator; inspect the profile validation
requirements in the source before connecting new hardware.

## See which AI is ready for the next task

<p align="center">
  <a href="docs/images/officestra-gui-mode.png"><img src="docs/images/officestra-usage-detail.png" alt="Actual whiteboard detail showing usage limits, reset times, and API-equivalent costs for three providers" width="480"></a>
</p>

Check provider limits and reset times before assigning the next task.
Review work activity, answers, and costs in conversations, and find past records when needed.<br>
*A detail from the original whiteboard screenshot. Amounts are API-equivalent costs, not subscription charges.*

## Compare costs and work ratings

<p align="center">
  <a href="docs/images/officestra-statistics.jpg"><img src="docs/images/officestra-statistics.jpg" alt="Actual usage statistics: cost summary, coworker and model ratings, and daily cost chart" width="100%"></a>
</p>

See when costs rise and which coworker–model combinations earn better feedback, all in one view.<br>
*Actual Claude Code statistics. Displayed amounts are not subscription charges.*

Models, reasoning options, and usage information depend on your account and CLI version.
This README describes features on the current `main` branch.

## Get started

The latest DMG release is **v1.5.0**. To run from source, use the AI-assisted setup below.

### Easiest path: ask an AI to do it

Send the following request to Codex, Claude Code, or Antigravity if you already
use one of them:

> “Download `https://github.com/neosp8888-design/officestra.git` to this Mac and run
> OFFICESTRA. Preserve my existing AI CLI logins and projects, inspect the
> environment first, and install only missing dependencies. Verify that both the
> app and its local backend are running.”

### Download the app

[Download OFFICESTRA v1.5.0 DMG](https://github.com/neosp8888-design/officestra/releases/tag/v1.5.0)
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
- Xcode Command Line Tools with Git and Swift 6.0 or later
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
