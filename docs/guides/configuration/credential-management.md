# Credential management

A Credential is a local authentication secret referenced by a Provider. Rivo never prints the secret and does not place it in `config.toml`, command arguments, logs, diagnostics, or session history.

## Save and inspect a Credential

```text
rivo config credentials path
rivo config credentials set <credential-name>
rivo config credentials show
```

Interactive `set` asks for the value twice with masked input. Names may contain only ASCII letters, digits, `_`, and `-`. `show` lists names and storage state; it never displays secret text.

For automation, read the value from an environment variable without putting it in the command line:

```text
rivo config credentials set openai-key --from-env OPENAI_API_KEY
```

The environment variable is read once by the command and its name is not saved as a Provider setting.

## Choose runtime authentication

A Provider can reference one stored Credential, or a run can supply an environment variable name:

```text
rivo config provider set gateway --base-url https://api.example.com/v1 --credential-name openai-key
rivo run --api-key-env OPENAI_API_KEY --prompt "Reply only: ok"
```

For a run, a non-empty `--api-key-env` value has priority. If it is missing, blank, or unreadable, Rivo falls back to the Provider's Credential reference. If a configured Credential cannot be read, the run fails with a redacted error; it does not try another secret.

## Migrate and remove

```text
rivo config credentials migrate
rivo config credentials unset <credential-name>
```

`migrate` converts legacy plaintext credential storage to the current encrypted file format. It does not migrate model configuration or sessions. A Credential referenced by any Provider cannot be removed; update or remove those Providers first. Removing a Provider never removes its Credential.

The file is locally obfuscated/encrypted, but it is not an OS keychain and does not protect against a compromised local account. CI and shared environments should prefer short-lived environment injection.

[Configuration index](README.md) | [Provider management](provider-management.md)

---

[简体中文](credential-management.zh-CN.md)
