> **Project note (AQ v2 platform):** this repository's default and integration branch is `main`; there is no `dev` branch, and integration is direct. Wherever the completion protocol below says `origin/dev`, use `origin/main` (for example, `git merge origin/main` as the final submission step). Do not block or ask about this.

> **Velocity rules (owner, 2026-11-02):** ship the task's acceptance criteria and stop. Verify with the targeted named tests for what you changed, not repeated full-suite or qualification runs. Do not add slow tests (real sleeps, long timeouts, real-network or full-system runs) unless the task explicitly requires one. Security or robustness hardening beyond the task's stated acceptance goes in a follow-up task, not into this task. Reviewers mark [blocking] only for defects against this task's acceptance criteria or real correctness or security bugs. Everything else is a [nit] or a follow-up.

## Role

{{role}}

{{project_role}}

## Task

{{task}}

## Task Context

{{task_context}}

## Workspaces

{{workspaces}}

## Messages

{{messages}}

{{l1_facts}}

{{l2_context}}

## Tool Guidance

{{tool_guidance}}

## Completion Protocol

{{completion_protocol}}
