# Contributing

Thanks for helping improve `subagent`.

Use a current stable Rust toolchain. Keep operational output compact JSONL, preserve
the agent-first command contract, and update `SKILL.md`, `references/protocol.md`, and
`references/cli.schema.json` together whenever a public shape changes.

Before opening a pull request, run:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
node --test tests/ui_core.test.js
cargo build --release --locked
SUBAGENT_BIN="$PWD/target/release/subagent" tests/e2e.sh
```

Do not include credentials, local state, generated review media, or project workspaces.
Security issues belong in a private GitHub security advisory rather than a public
issue.

Maintainers publish releases from a clean `vMAJOR.MINOR.PATCH` tag after CI passes.
The tag version must match `Cargo.toml`; the release workflow builds the static Linux
x86-64 artifact, publishes its checksum, and verifies the latest installer.

The README animation is reproducible from source. With `vhs`, `ttyd`, `tmux`, and
`ffmpeg` on `PATH`, build the host release and run:

```sh
vhs docs/cli-demo.tape
```

The tape uses the local deterministic API fixture, isolated XDG directories under
`/tmp`, and `scripts/demo-session.sh`; it does not use credentials or paid requests.
