# Agent Instructions

## Git history

- Keep repository history linear. Create feature branches from the latest `origin/main`.
- Rebase onto the latest `origin/main` before every push, and update the remote with `git push --force-with-lease` after a rebase.
- Never integrate a PR with a merge commit. Use rebase merge; for a PR containing noisy intermediate commits, squash merge is acceptable only when the resulting history remains linear.
- A linear visual graph is part of code review: branches and PR commits must not introduce parallel histories.
