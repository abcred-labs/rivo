# Source configuration and testing

Run source commands from the runtime directory:

~~~powershell
Set-Location runtime
cargo run -p rivo -- --help
cargo run -p rivo -- config credentials set my-provider-key
cargo run -p rivo -- config provider set openai --base-url https://api.openai.com/v1 --credential-name my-provider-key
cargo run -p rivo -- config validate
~~~

Provider keys must be entered through the credential command or supplied for one run with an environment variable. Never commit key material or place it in command history, TOML, logs, screenshots or test reports.

## Deterministic checks

~~~powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
Set-Location ..
python scripts/check_markdown_links.py
~~~

The xtask quality gate combines the repository checks:

~~~powershell
rustup toolchain install 1.88.0 --profile minimal
cargo run -p xtask -- check
~~~

## Local mock provider

Use packaging/archive/README.md for a credential-free loopback provider example. It is deterministic and does not contact an external model service.

## Real providers

After configuring a provider, refresh its exact model catalog before running a task:

~~~powershell
cargo run -p rivo -- config provider models refresh <provider-id>
cargo run -p rivo -- config route --vendor <vendor> --model <exact-model-id> --provider-id <provider-id>
cargo run -p rivo -- run --vendor <vendor> --model <exact-model-id> --provider-id <provider-id> --prompt "Reply only: RIVO_OK"
~~~

Real-provider tests can incur usage charges and require credentials. Do not include their endpoints or outputs in public fixtures.
