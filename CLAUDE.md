# CLAUDE.md

See [AGENTS.md](AGENTS.md). In short: the open base tier — crates here are
depended on by everything above and depend on nothing above; promote a crate only
when ≥2 components actually share it (rule of three); generic mechanism only, no
product domain or secrets; decisions recorded as ADRs under [docs/adr](docs/adr/README.md).
