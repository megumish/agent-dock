# agent-dock

> A local control plane for evaluating and routing AI agents using evidence from your own work.

agent-dock is an experimental multi-agent environment designed to stay under the user's control. It treats each combination of an AI model, tools, permissions, and execution environment as an **execution profile**, evaluates those profiles on real work, and uses the resulting evidence to help decide which profile should handle future tasks.

The goal is not to find a universally "best" model. It is to answer a more practical question:

> For this kind of task, which model and configuration are sufficient to produce acceptable results at an appropriate cost?

External benchmarks are useful prior information, but they do not reflect every user's workflows, tools, permissions, prompts, or cost constraints. agent-dock therefore builds its primary evidence from the user's own work.

The user's judgment remains authoritative. agent-dock can advise, record, and route, but the final decision stays with the user.

## How it works

agent-dock connects **evaluation** and **routing** in one loop:

1. Register execution profiles.
2. Give agent-dock a task and its tags.
3. agent-dock uses prior evidence to suggest a profile, or defers when the evidence is insufficient or ambiguous.
4. The user approves the suggestion or chooses another profile.
5. agent-dock either launches the selected CLI or observes a directly started session.
6. The result is judged and recorded as an evaluation event.
7. Those events are projected into scorecards that inform future routing decisions.

An execution profile is more than a model name. It represents the combination of model, provider, tools, permissions, and execution environment used for the work. This avoids attributing improvements from tooling or configuration changes to the underlying model itself.

### Local control

"Local" refers to control of the evidence, evaluation, and routing rules. Inference itself may happen locally or through a hosted API.

agent-dock keeps its evaluation records locally and owns the boundary to external execution systems; it does not own the agents' output artifacts themselves.

## Current status: 0.1.0

0.1.0 is the first usable release containing the full basic evaluation-and-routing loop.

It currently supports:

- **Mediated headless execution** of Claude Code, Codex CLI, Gemini CLI, and Antigravity CLI using their machine-readable interfaces.
- **Observation of directly started sessions** for Claude Code and Codex CLI through hooks.
- **Append-only local evaluation events** for routing decisions, executions, attribution changes, and user judgments.
- **Scorecards** derived from recorded evidence.
- **Routing advice** based on evidence from tasks with matching tags.
- **Explicit deferral** when there is not enough evidence or no unique candidate can be selected.

The supported OS for 0.1.0 is **macOS**. Other operating systems are not currently guaranteed to work.

For mediated executions, agent-dock records elapsed time together with usage and cost values reported by the underlying CLI when available. These reported values are kept separate from actual monetary spend. 0.1.0 does not obtain authoritative actual-spend data and records it as missing instead of estimating it.

See [`docs/releases/0.1.0.md`](./docs/releases/0.1.0.md) for the exact behavior and limitations of the release.

## Quick start

The Rust toolchain is managed with [mise](https://mise.jdx.dev/).

```console
mise install
mise run
```

On first launch, agent-dock can create four headless execution profiles plus interactive profiles for Claude Code and Codex CLI. The model, permission, and sandbox behavior used by each CLI continues to follow that CLI's normal configuration.

### Judge a previous run

```console
mise run default -- -- judge
```

### View scorecards

```console
mise run default -- -- scorecard
```

Mediated runs and attributed direct runs appear in the same scorecard system.

## Observing direct CLI sessions

agent-dock can collect evaluation metadata from sessions that the user launches directly, without forcing every task through agent-dock itself.

### Claude Code

Pass hook input from Claude Code's `SessionStart`, `SessionEnd`, `CwdChanged`, and `StopFailure` hooks to:

```console
agent-dock observe hook claude
```

### Codex CLI

Pass hook input from Codex CLI's `SessionStart`, `SessionEnd`, and `Stop` hooks to:

```console
agent-dock observe hook codex
```

Follow each CLI's official instructions for configuring command hooks and trust/permission settings.

The hook integration normalizes only the allowed session identifier, working directory, model, and permission mode. It does **not** store conversation transcript paths, prompts, responses, tool arguments, or tool outputs.

Task boundaries are explicit rather than inferred automatically. If only an external session ID is known, first resolve the local session ID:

```console
agent-dock observe session-find --cli codex --external-id <external-session-id>
agent-dock observe task-start --session <local-session-id> --tag rust
agent-dock observe task-complete --session <local-session-id>
```

Use `task-interrupt` to close an interrupted task.

If hook metadata is insufficient for automatic attribution, pass `--profile '<interactive-profile-name>'` to both `task-start` and `task-complete`. Incorrect attribution can later be corrected with `task-attribute` or `task-unattribute`; corrections are appended as new events rather than rewriting the previous history.

## Documentation

The project's normative and implementation documents have distinct roles:

| Document | Role |
| --- | --- |
| [`VISION.md`](./VISION.md) | Principles, conceptual model, success criteria, and non-goals. This is the highest-level normative document. |
| [`DESIGN.md`](./DESIGN.md) | Responsibilities, invariants, constraints, and scope for 0.1.0. |
| [`DEPENDENCIES.md`](./DEPENDENCIES.md) | Project-wide policy for adopting, updating, and reevaluating external crates. |
| [`docs/releases/`](./docs/releases/) | Historical record of behavior, capabilities, limitations, and changes in each release. |
| `docs/adr/` | Architectural decision records for changes from 0.2.0 onward. |
| [`AGENTS.md`](./AGENTS.md) | Working instructions and constraints for coding agents contributing to the repository. |
| GitHub Issues | Open questions and possible future work that have not yet become normative design decisions. |

A useful reading order is:

1. [`VISION.md`](./VISION.md)
2. [`DESIGN.md`](./DESIGN.md)
3. [`DEPENDENCIES.md`](./DEPENDENCIES.md)
4. [`docs/releases/0.1.0.md`](./docs/releases/0.1.0.md)

Most of the detailed design documentation is currently written in Japanese.

## Design principle

agent-dock is deliberately evidence-driven and user-controlled. It should not silently turn weak evidence into confident routing decisions, and it should not replace the user's value judgments with model-generated ones.

When the evidence is insufficient, uncertainty is part of the answer.
