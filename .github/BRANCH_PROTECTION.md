# Main Branch Protection

## Desired state

The desired protection for `oh-my-harness/llm-harness-runtime` is the active branch ruleset `main-protect` (ruleset ID `17595071`). The complete desired payload is `.github/rulesets/main-protection.json`.

The desired payload:

- targets `refs/heads/main` only;
- keeps deletion, non-fast-forward, and linear-history protection;
- requires a pull request and allows only rebase merges;
- preserves the existing pull request review settings;
- requires these status checks with strict up-to-date semantics:
  - `Rust (ubuntu-latest)`
  - `Rust (windows-latest)`
  - `Rust (macos-latest)`
- grants no bypass actors.

This file describes desired state only. This repository change did not apply or validate the ruleset on GitHub, and it did not run or validate CI. The remote ruleset remains an external administrative concern until an administrator performs and verifies the update below.

## Administrator access

Applying the payload requires repository administrator access. The authenticated account or fine-grained token must have write permission for the repository's **Administration** setting, and GitHub authorizes the request server-side. Do not store credentials or tokens in this repository.

## Read the current ruleset

The following command is read-only:

```sh
gh api --method GET repos/oh-my-harness/llm-harness-runtime/rulesets/17595071 --jq '{name,target,enforcement,bypass_actors,conditions,rules}'
```

Use the result to review the currently active ruleset before making any change. The `GET` command does not modify GitHub.

## Apply the desired ruleset

The following command is a mutating external operation and must be run only by an authorized repository administrator after reviewing the desired payload:

```sh
gh api --method PUT repos/oh-my-harness/llm-harness-runtime/rulesets/17595071 --input .github/rulesets/main-protection.json
```

The `PUT` request replaces the ruleset configuration with the complete payload; it is not a partial patch. Re-run the read-only `GET` command afterward to verify the resulting remote configuration.

This update is outside the scope of this repository change. It must not be treated as applied, and the required checks must not be treated as passing, until an administrator has completed and verified the external operation.
