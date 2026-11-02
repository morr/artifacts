# artifacts

## Skills

Project skills live in `skills/` (currently `artifact-builder`) and are consumed by
agent sessions through symlinks from the agent skill directories
(`~/.agents/skills/artifact-builder`), so a skill is edited in this repo only.
`design.md` and `claude-brand.md` inside the skill are symlinks into the
`vendor/anthropic-skills` submodule — never edited in place, only bumped as a whole
(see Sources in the skill).

A skill carries what the code itself will not say — mechanisms, invariants, traps,
canon, and decision rules; the repository code is the working example. Nothing
reproducible by grepping the repository goes into a skill: constant values,
enumerations of helpers and their signatures, flag tables, code examples duplicating
files and concerns. The check applies to every line when a skill is written or
updated: a line derivable from reading the code is a candidate for deletion.
