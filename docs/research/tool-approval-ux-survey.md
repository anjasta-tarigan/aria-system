# Tool Approval / Permission UI — Competitive Survey

> Research date: November 2026  
> Purpose: Understand how leading AI coding assistants handle tool approval, permission levels, and adaptive UI, to inform Aria's own approval chip/selector design.

---

## 1. Claude Code (Anthropic)

### Sources
- https://docs.anthropic.com/en/docs/claude-code/permissions
- https://code.claude.com/docs/en/permission-modes

### Permission levels (modes)

Claude Code has **six named permission modes**, cycled with `Shift+Tab` in the CLI or via a **mode selector below the prompt box** in the VS Code extension and Desktop app:

| Mode | Label shown | What auto-runs |
|---|---|---|
| `default` | **Manual** | File reads in working dir; nothing else |
| `acceptEdits` | **Edit automatically** | Reads + file edits + common FS commands (`mkdir`, `mv`, `cp`, etc.) |
| `plan` | **Plan** | Reads + classifier-approved shell commands; file edits blocked until plan approved |
| `auto` | **Auto** | Everything — a classifier subagent reviews actions in background |
| `dontAsk` | _(headless only)_ | Pre-approved tools only; anything that would prompt is denied |
| `bypassPermissions` | **Bypass permissions** | Everything, no classifier — for isolated containers/VMs |

### UI controls

- **CLI**: a status bar at the bottom shows the active mode as a gray badge: `⏸ manual mode on`, `⏵⏵ auto mode on`, `⏵⏵ accept edits on`, etc. Pressing `Shift+Tab` cycles modes. Mode names appear directly in the badge text.
- **VS Code extension**: a **mode indicator at the bottom of the prompt box** (a clickable chip/dropdown). Labels: Manual / Edit automatically / Plan / Auto / Bypass permissions.
- **Desktop app (claude.ai/code)**: a **mode selector in a row under the prompt box**. Same labels. Auto only appears when available for the account/plan.
- **Web (claude.ai/code)**: a **mode dropdown in a row under the prompt box**. Cloud sessions only show Accept edits / Plan / Auto (no Manual, no Bypass).
- **Mobile**: tap the **+** button in the prompt box → **Permission** to reach the same selector.

### Per-action approval prompt

When Manual mode triggers a prompt, it shows:
1. A description of the action ("Run the test suite")
2. The exact command (`npm test`)
3. Four options:
   - **Yes** (one-time)
   - **Yes, and don't ask again for: `npm test *`** (saves a rule to `.claude/settings.local.json` permanently for Bash/WebFetch; session-only for file edits)
   - **Yes, and switch to auto mode** (appears on eligible prompts; upgrades the session)
   - **No**
4. `Tab` to add a comment to the approval/denial
5. `Esc` to cancel

### Adaptive / "smart" behavior

- **Auto mode is now the default** for new interactive sessions on v2.1.283+ (all plans). A one-time notice appears the first time the built-in default starts a session in auto mode.
- When in Manual or acceptEdits mode, if auto mode is available, the prompt offers **"Yes, and switch to auto mode"** as an escape hatch — this is contextual and only appears on Bash prompts, not WebFetch.
- **Plan mode**: at the end of planning, presents a plan approval prompt with three options: approve + use auto mode / approve + manually review edits / keep planning.
- Selecting a Daybreak security model in the ChatGPT desktop app **automatically switches** the permissions control to "Approve for me" if that mode is available for the account — a model-driven mode switch.
- The status bar badge changes appearance per mode (pause icon for manual, fast-forward for auto/acceptEdits).
- Outside a git repo or on Windows, the "don't ask again" rule may be saved to a different path.

### Context-sensitivity

- **Working directory awareness**: reads inside the working directory and additional directories are always free; reads outside require approval or an allow rule.
- **Protected paths**: `.git`, `.claude`, etc. trigger prompts even in `bypassPermissions`.
- **Critical-path `rm`**: always prompts regardless of mode — no mode auto-approves `rm -rf /` or `rm -rf ~`.
- **Ask rules** in settings force a prompt even in auto mode.

---

## 2. ChatGPT / Codex (OpenAI)

### Sources
- https://learn.chatgpt.com/docs/permission-modes
- https://learn.chatgpt.com/docs/sandboxing/auto-review

### Permission levels (modes)

ChatGPT (desktop app) and Codex (CLI/IDE) offer **four permission modes** via a **permissions control below the composer**:

| Mode | Sandbox | Approvals | Notes |
|---|---|---|---|
| **Ask for approval** | `workspace-write` | Manual — pauses before external file edits and internet access | Default for most work |
| **Approve for me** | `workspace-write` | AI reviewer (auto-review) handles eligible requests | Same sandbox boundary as "Ask for approval" |
| **Full access** | None | Unrestricted internet + any file | Requires model-specific warning before enabling |
| **Custom (config.toml)** | From config | From config | Per-project or org config |

### UI controls

- **Desktop app**: a **permissions control below the composer** — a dropdown/selector widget. User opens it and selects a mode.
- **CLI**: type `/permissions` to open an interactive dialog.
- The control shows the selected mode per-chat; having a mode available ≠ it being selected.

### Auto-review (AI reviewer)

"Approve for me" routes eligible approval requests to a **separate reviewer agent** instead of showing a manual prompt:

1. Main agent proposes an action (e.g., run a command).
2. If action needs approval, Codex creates a request.
3. Reviewer agent (separate model) evaluates the request against a policy.
4. If approved, execution continues. If denied, main agent must find a safer path or stop.

**What auto-review blocks by default**: sending private data/secrets to untrusted destinations, probing for credentials, persistent security weakening, destructive irreversible actions.

**Interruption override**: The TUI has a `/approve` command to open an "Auto-review Denials" picker and manually approve one recent denied action for one retry.

**Consecutive-denial limit**: After 3 consecutive denials (or 10 in last 50), the CLI interrupts the turn and stops. Security models interrupt on first denial.

### Context-sensitivity / adaptive behavior

- Selecting an **approved Daybreak security model** in the desktop app **automatically switches** the permissions control to "Approve for me" if that mode is available — the UI adapts to the model selected.
- Before enabling **Full access** for a Daybreak security model, the app shows a **model-specific warning** recommending "Approve for me" instead.
- **Cloud sandboxes** (GitHub-hosted environments) completely bypass run modes — no approval needed inside the sandbox.
- Sandbox and approvals are orthogonal controls: changing who reviews doesn't change sandbox boundary.

---

## 3. Cursor AI

### Sources
- https://cursor.com/docs/agent/security/run-modes
- https://cursor.com/changelog/auto-review (v3.6, May 29 2026)
- https://cursor.com/blog/agent-autonomy-auto-review

### Permission levels (run modes)

Cursor uses the term **"Run Modes"** for agent autonomy levels. Configured in **Settings → Agents → Approvals & Execution**:

| Mode | Auto-runs | Sandbox | Classifier | Use case |
|---|---|---|---|---|
| **Auto-review** | Allowlisted calls + sandboxable shell commands | Yes (for shell) | Yes (classifier subagent) | Recommended default: fewer prompts + safety review |
| **Allowlist** | Actions in allowlist | Optional | No | Deterministic trusted repeat actions |
| **Run Everything** | Every tool call automatically | No | No | Zero prompts, full trust |

Earlier (v3.5): "Ask Every Time" was deprecated; "Run in Sandbox" was folded into Allowlist.

### File edit approval (diff UI)

Cursor has a separate layer for **file edit approvals** — an inline diff view that shows proposed changes. The user accepts or rejects the diff. This is distinct from shell/MCP/Fetch approvals. In Auto-review mode, file edits inside the workspace are auto-applied; only shell, MCP, and Fetch calls go through the classifier or approval flow.

### Auto-review classifier (Gemini 3.5 Flash Lite, Haiku fallback)

The classifier is a **specialized subagent** that runs before a tool call executes:

1. Allowlisted calls → run immediately (no classifier).
2. Shell commands that can run sandboxed → run in sandbox.
3. Everything else → classifier reviews (can itself run ReadFile, Grep, Glob, ListDir to inspect the workspace before deciding).
4. If blocked: agent tries another approach or shows an approval prompt.

**Key stats** (from Cursor blog, Jun 2026):
- Only ~4% of classifier-reviewed actions get blocked.
- ~7% of total chats in Auto-review see at least one user interruption.
- Previous enterprise baseline: ~40% of actions blocked.
- Auto-review reduces approval prompts by ~84%.

### Configuration

- `permissions.json` (plain-English instructions, e.g., "Every AWS CLI command should go through approval first") at `~/.cursor/permissions.json` (global) or `<project>/.cursor/permissions.json` (project-scoped, can be committed).
- `sandbox.json` controls network allowlist and readable/writable paths.
- Admins can set a **team-wide Auto-review config** in the dashboard that takes priority over user/project files.

### UI controls

- **Settings → Agents → Approvals & Execution**: radio button/selector for run mode.
- Approval card appears inline in chat when classifier routes to user.
- **Additional per-action protections** (always require approval, even in Run Everything): Browser Protection, File-Deletion Protection (`rm` commands), External-File Protection (files outside workspace).

### Adaptive / context-sensitive behavior

- Auto-review is now the **default for new users**; existing users opt in via Settings → Agents.
- Classifier is **context-aware**: inspects workspace before deciding (e.g., reads a script before approving `python script.py`).
- Sandboxed commands that **fail on a sandbox restriction** can be retried outside the sandbox — but that rerun goes to the classifier.
- If the classifier model (Gemini 3.5 Flash Lite) is blocked by enterprise model access controls, **Auto-review is automatically disabled** and falls back to Allowlist mode.

---

## 4. Windsurf / Devin Desktop (Cognition / formerly Codeium)

### Sources
- https://docs.windsurf.com/windsurf/cascade/modes (Devin Local Modes)
- https://docs.devin.ai/desktop/devin-local/permissions
- https://docs.devin.ai/desktop/terminal

> **Note**: Windsurf was acquired by Cognition in 2025 and renamed to **Devin Desktop** on June 2, 2026. The docs now live at docs.devin.ai / docs.windsurf.com (redirects). The underlying agent is "Devin Local."

### Permission levels (modes)

Five modes, switched via a **mode selector below the input box** or via `/code`, `/smart`, `/ask`, `/plan`, `/bypass` slash commands:

| Mode | File edits | Shell commands | MCP/Fetch |
|---|---|---|---|
| **Code** (default) | Auto-approved inside workspace | Prompt unless allow rule | Prompt unless allow rule |
| **Smart** | Auto-approved inside workspace | Fast model auto-approves "safe" calls | Fast model auto-approves "safe" calls |
| **Ask** | Not allowed (read-only) | Not allowed | Not allowed |
| **Plan** | Blocked until plan approved | Read-only exploration only | Read-only |
| **Bypass Permissions** | Auto-approved | Auto-approved | Auto-approved |

**Sandboxed sessions** use a different "Autonomous" mode: OS-level sandbox limits what commands can reach, and you're prompted for capabilities (e.g., network access) rather than individual commands.

### UI controls

- **Mode selector** below the input box in Devin Desktop — a dropdown or button group showing: Code / Smart / Ask / Plan / Bypass Permissions.
- **Command card** (approval request card): appears inline when agent wants to run a command that needs approval. The card offers:
  - Run once
  - Allow for the rest of the session
  - Allow permanently (adds a rule to settings)
  - Edit the command before approving (click command text)
  - **Wand action** on the edit field: describe a change in plain language and a fast model rewrites the command for review
  - Keyboard shortcuts for approve / always-allow / reject
- **MCP permission card**: when agent calls an MCP tool, offers:
  - Allow this specific tool (once / session / permanently)
  - Allow all tools on this MCP server (session / permanently)
- **Allow/Deny list** in Terminal UI: for a command card that has already run, open **Auto-run settings** (settings icon on the card) and choose Prefix (`git *`) or Allow (exact command).

### Session-wide grants

An approval granted for the session propagates to all subagents — root agent and subagents don't re-prompt for a scope already granted.

### Enterprise controls

Admins can enforce rules, sandbox mode, and network rules for every member. Admin-enforced deny/ask rules override Bypass Permissions.

### Adaptive behavior

- **Smart mode**: a fast classifier model auto-approves shell commands it judges safe; high-risk categories (package installs, mutating git, `rm`, `sudo`, destructive cloud CLI, sensitive files) always prompt.
- **Sandboxed session**: OS-level sandbox replaces granular action approval with capability-level approval. The mode UI changes entirely — no Code/Smart/Ask modes, just "Autonomous."
- **MCP server-level grants**: recent changelog added server-level MCP approval (approve all tools on a server), not just tool-level — a UX improvement that reduces prompt fatigue for trusted integrations.

---

## 5. GitHub Copilot (GitHub)

### Sources
- https://docs.github.com/en/copilot/how-tos/github-copilot-app/agent-sessions
- https://docs.github.com/en/copilot/concepts/customizations/allowing-safe-commands

### Permission levels

GitHub Copilot app (desktop) has two orthogonal controls:

**Session mode** (what the agent does autonomously):

| Mode | Behavior |
|---|---|
| **Interactive** | Agent suggests changes and waits for input before proceeding |
| **Plan** | Agent creates a plan first; user approves before execution |
| **Autopilot** | Agent works fully autonomously — writes, tests, iterates without input |

**Tool permissions** (how approval of individual tools works):

| Mode | Behavior |
|---|---|
| **Manual approval (Ask every time)** | Approval required before every tool use |
| **Assisted** | Model assesses each request; auto-approves safe ones, prompts for unsafe |
| **Unconditional approval (Approve all)** | No approval prompts at all |

### UI controls

- Both controls are **dropdowns below the prompt field** in the app.
- To change tool permissions per session: click the mode indicator → hover **Tool permissions** → select.
- Default for future sessions: app settings → Sessions → Tool permissions.
- When a tool needs approval in Manual mode: a prompt card appears with options to allow **for this session** or **always** (persisted across sessions).
- VS Code agent mode: separate approval flow — a prompt appears in the chat panel before each tool call.

### Sandboxes

- **Cloud sandbox**: session runs in GitHub-hosted isolated environment. No approval needed from Run Modes (the sandbox boundary is the control).
- **Local sandbox**: OS-level restrictions on the local machine. Configured per-session with `/sandbox on` / `/sandbox off`.

### Adaptive behavior

- Copilot Fusion (model selection): automatically selects the right model per task; model choice doesn't affect the permission control.
- "Assisted" mode uses a model to assess each request — same pattern as Cursor Auto-review and Codex "Approve for me."

---

## Cross-cutting patterns

### 1. The "dial not a switch" trend (2025–2026)

All major tools have converged from binary (ask/don't ask) to a **spectrum** of autonomy modes with AI-classifier as a middle tier:

```
Manual (ask everything)
  → Accept Edits / Code (auto-approve file writes, ask shell)
    → Smart / Auto-review / Assisted (AI classifier gates shell/MCP/Fetch)
      → Full Auto / Bypass / Run Everything (no prompts)
```

The classifier tier (Claude auto mode, Cursor Auto-review, Codex "Approve for me", Windsurf Smart, Copilot Assisted) is now the **recommended default** at every tool.

### 2. Where the control lives

| Tool | Control location |
|---|---|
| Claude Code CLI | Status bar badge (bottom of terminal), `Shift+Tab` to cycle |
| Claude Code VS Code | **Chip/button at bottom of prompt box** |
| Claude Code Desktop | Mode selector row **below the prompt box** |
| ChatGPT / Codex Desktop | **Permissions control below the composer** |
| Cursor | **Settings → Agents → Approvals & Execution** (not inline in chat) |
| Windsurf / Devin Desktop | **Mode selector below the input box** (inline in chat panel) |
| GitHub Copilot App | **Dropdowns below the prompt field** |

The dominant pattern is a **selector/dropdown below the composer**, not a toggle or header chip. The VS Code extension pattern tends to be a small text label or chip at the bottom edge of the prompt input area.

### 3. Per-action card anatomy

When an approval prompt appears mid-task, all tools show a card that includes:
- What the action is (description + exact command/file path)
- Options: allow once / allow for session / allow permanently
- Some tools add: edit the command, add a comment, keyboard shortcuts

Unique additions:
- **Claude Code**: "Yes, and switch to auto mode" — upgrades mode from the prompt itself.
- **Windsurf / Devin**: wand/magic action to rewrite the command in plain language before approving.
- **Codex CLI**: `/approve` command to approve one recent auto-review denial for one retry.

### 4. Context-sensitivity / adaptive changes

| Trigger | Tool | Adaptation |
|---|---|---|
| User selects a Daybreak security model | ChatGPT Desktop | Auto-switches to "Approve for me" mode |
| Session starts in a cloud sandbox | GitHub Copilot, Codex | No approval UI needed; sandbox is the control |
| Session starts in a sandboxed container | Windsurf | Mode selector changes to "Autonomous" mode |
| Classifier model is blocked by enterprise policy | Cursor | Auto-review is disabled; falls back to Allowlist |
| Session is a CI/headless run | Claude Code | Defaults to Manual or dontAsk; auto mode not available without feature flags |
| First time auto mode becomes the default | Claude Code | One-time notice shown in terminal / VS Code onboarding card |
| File outside working directory | Claude / Cursor / Devin | Always prompts regardless of mode |
| `rm` / deletion commands | Claude / Cursor / Devin | Protected category — always prompts or uses a separate "File-Deletion Protection" toggle |

### 5. Escape hatches from prompts

Every tool provides ways to "stop being asked" without switching to full bypass:
- **Permanent allow rules** (glob-matched, file-scoped, domain-scoped) — Claude Code, Codex, Cursor, Devin
- **Session-wide grants** — Devin Local (propagates to subagents automatically)
- **"Don't ask again" on the prompt** — Claude Code's "Yes, and don't ask again for: `npm test *`"
- **Allowlists** (prefix or exact match) — Cursor, Devin
- **Custom classifier instructions** (plain-English "always block" / "always allow") — Cursor `permissions.json`

### 6. What no tool does yet

- **Context-aware UI adaptation based on folder trust** (e.g., auto-elevating permissions when inside a sandboxed temp directory vs. the home directory) — the concept exists in sandbox rules but doesn't surface in a visual adaptive chip.
- **Visual risk indicator** on the approval chip (e.g., the chip changes color or shows a warning badge when the pending action is classified as high-risk).
- **Mode auto-downgrade** when a risky pattern is detected mid-session (all tools only upgrade/downgrade on explicit user action or model selection).
