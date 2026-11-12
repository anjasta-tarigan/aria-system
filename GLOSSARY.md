# Glossary

## Workspace

The single directory a conversation is bound to. It is the agent's whole world: the shell runs inside it, and every file tool refuses paths that resolve (after symlinks) outside it.

_Avoid_: project, folder root, cwd.

## Conversation

An ordered transcript of user and assistant messages, bound to exactly one Workspace and one model configuration. It is the unit the user opens, searches and resumes.

_Avoid_: session, chat, thread.

## Run

One execution of the agent loop for a Conversation, started by a user message and ending when the model stops calling tools, a cap is reached, or the user cancels. A Run can outlive the window: closing the UI does not stop it.

_Avoid_: task, job, turn sequence.

## Turn

One request/response pair with the model inside a Run. A Turn may contain several Tool Calls and their results. A Run is a sequence of Turns.

_Avoid_: step, iteration, round.

## Tool Call

A request from the model, in a Turn, to invoke one of the agent's Tools, with a JSON argument object. Its outcome is a Tool Result.

_Avoid_: function call, action, command.

## Tool Result

The outcome of a Tool Call fed back to the model in the next Turn. A Tool Result from a failing Tool is a normal result (marked as an error) that the model is expected to recover from; it does not end the Run.

_Avoid_: tool output, response.

## Tool

A capability the agent can invoke: reading, listing, searching, editing, writing, deleting or moving files, running a shell command, fetching a web page, and maintaining the Run's todo list. Each Tool has an approval class (read, mutate, exec, network).

_Avoid_: function, skill, plugin.

## Edit

A change to an existing file expressed as an exact search string replaced by a replacement string. An Edit applies only if its search string matches exactly once; anything else is a Tool Result error.

_Avoid_: patch, diff, hunk, modification.

## Approval

A pending decision, recorded in the database, asking the user to allow one Tool Call whose class is mutate, exec or network. It has no timeout: it waits until the user answers or the Run is cancelled.

_Avoid_: permission, consent, confirmation dialog.

## Undo

Restoring a file to its state before the agent's last change to it, by copying the pre-change content (kept in a blob store) back into place.

_Avoid_: revert, rollback, checkpoint.

## Cap

A hard limit on a Run (number of Turns, prompt tokens) that ends it with a "budget exhausted" outcome the user can continue from. A soft warning is issued before the Cap is reached.

_Avoid_: budget, limit, quota.

## Provider

The external OpenAI-compatible endpoint that serves the model: a base URL, an API key and a model name.

_Avoid_: backend, LLM service, model (the model is one of the Provider's settings).
