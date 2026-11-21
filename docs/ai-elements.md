# AI Elements (shadcn/ui) — Reference Summary

Source: https://ui.shadcn.com/docs/ai-elements

## Overview

AI Elements are patterns for building agentic (AI chat) interfaces. Each one is a **composition of shadcn/ui components** that you copy into your project — there is **no package to install**. The code is yours.

Core model:
- A **Chat** is a list of **Message** turns inside a **Message Scroller**.
- The steps an agent takes inside a turn (Reasoning, Tool, Plan) are built on **Trace**, so every step has the same shape.
- Recommended starting point: the [chatbot template](https://github.com/shadcn-ui/chatbot-template) (Next.js app with AI SDK + shadcn/ui, streaming chat, tool calls, web search, clarifying questions).

## Patterns

| Element | Description |
|---|---|
| **Chain of Thought** | A summary that expands into the steps an agent took. |
| **Chat** | The thread; Message Scroller keeps your place while the reply streams. |
| **Checkpoint** | A point in the conversation you can return to. |
| **Confirmation** | A prompt to approve or deny a tool before it runs. |
| **Context** | How much of the context window the conversation has used. |
| **File Tree** | The files the agent changed, as a folder tree. |
| **Inline Citation** | A source in the sentence that opens on hover. |
| **Message** | One turn: the person's message, and the agent's reply with its actions. |
| **Model Selector** | Two menus: one picks the model, the other picks how hard it thinks. |
| **Plan** | The steps the agent plans to take, and how far it has got. |
| **Prompt Input** | A message field with attachments and a send button. |
| **Queue** | The agent's todo list, split into what is left and what is done. |
| **Reasoning** | The agent's thinking as it streams. |
| **Sources** | The links the agent used for a reply. |
| **Stack Trace** | A failed tool call and its stack frames. |
| **Subagent** | A subagent run, with its tool calls and result. |
| **Suggestion** | A short prompt you can send to the agent in one click. |
| **Task** | The steps the agent takes, as it takes them. |
| **Terminal** | A command the agent runs, with its output as it streams. |
| **Test Results** | A test run by the agent, with its suites and failures. |
| **Tool** | A tool call the agent makes, from input to output. |

## Migration from AI SDK Elements

Existing projects using elements.ai-sdk.dev do not need to migrate — the code was copied into the project and keeps working. To move a wrapper onto shadcn/ui components, use the migration prompt on each pattern page; it rebuilds the file on Trace, Code Block, and other shadcn/ui components while keeping every export and its props.

### Element → shadcn/ui replacement mapping

| AI SDK Elements | shadcn/ui replacement |
|---|---|
| `agent.tsx` | Subagent |
| `attachments.tsx` | Attachment |
| `chain-of-thought.tsx` | Chain of Thought |
| `checkpoint.tsx` | Checkpoint |
| `code-block.tsx` | Code Block |
| `commit.tsx` | Trace (commit) |
| `confirmation.tsx` | Confirmation |
| `context.tsx` | Context |
| `conversation.tsx` | Chat |
| `file-tree.tsx` | File Tree |
| `inline-citation.tsx` | Inline Citation |
| `message.tsx` | Message |
| `model-selector.tsx` | Model Selector |
| `plan.tsx` | Plan |
| `prompt-input.tsx` | Prompt Input |
| `question.tsx` | Questionnaire |
| `queue.tsx` | Queue |
| `reasoning.tsx` | Reasoning |
| `shimmer.tsx` | shimmer utility |
| `sources.tsx` | Sources |
| `stack-trace.tsx` | Stack Trace |
| `suggestion.tsx` | Suggestion |
| `task.tsx` | Task |
| `terminal.tsx` | Terminal |
| `test-results.tsx` | Test Results |
| `tool.tsx` | Tool |

### Elements without a direct replacement

These were not rebuilt (low usage). Each is small enough to build from existing shadcn/ui components:

| AI SDK Elements | Build it with |
|---|---|
| `artifact.tsx` | Card, Button, Tooltip |
| `audio-player.tsx` | `audio` element, Button Group, Slider |
| `canvas.tsx`, `connection.tsx`, `controls.tsx`, `edge.tsx`, `node.tsx`, `panel.tsx`, `toolbar.tsx` | React Flow, Card, Button Group |
| `environment-variables.tsx` | Item, Badge, Switch |
| `image.tsx` | `img` element |
| `jsx-preview.tsx` | A JSX parser (e.g. react-jsx-parser) |
| `mic-selector.tsx`, `voice-selector.tsx` | Combobox |
| `open-in-chat.tsx` | Dropdown Menu |
| `package-info.tsx` | Item, Badge |
| `persona.tsx` | Rive animation |
| `sandbox.tsx` | Trace, Tabs, Code Block |
| `schema-display.tsx` | Collapsible, Badge, Code Block |
| `snippet.tsx` | Input Group |
| `speech-input.tsx` | Button, Spinner, Web Speech API |
| `transcription.tsx` | Button per segment (to seek to it) |
| `web-preview.tsx` | `iframe`, Input Group for address bar |

## How to use

1. Add components:
   ```
   pnpm dlx shadcn@latest add trace code-block
   ```
2. Copy a pattern's code into your project (each page lists required components).
3. Or ask your coding agent: `Copy the <Element> pattern from https://ui.shadcn.com/docs/ai-elements/base/<element>.md to my chat.`

## Relevance to the Aria project

Aria already implements enterprise-grade CoT reasoning and Claude-style artifacts end-to-end (provider `reasoning_content` delta → DB `messages.reasoning` column → UI collapsed `reasoning-block`, toggled via `ui.expanded` keyed `reasoning-<msgId>`). The shadcn AI Elements patterns are a useful UI reference for aligning Aria's chat surface with established agentic-interface conventions — particularly:
- **Reasoning** (collapsed streaming thinking block) — matches Aria's `MessageView.reasoning` + collapsed-by-default block.
- **Chain of Thought** (expandable summary of agent steps).
- **Tool** (tool call card with input→output) — aligns with Aria's tool-call cards.
- **Task / Queue / Plan** (steps, todo list, plan progress).
- **Artifact** has no direct shadcn replacement; Aria's existing artifact card + split-pane panel (Code/Preview iframe, version switcher) is the custom implementation, buildable from Card + Button + Tooltip + Tabs + iframe per the "without replacement" guidance.
