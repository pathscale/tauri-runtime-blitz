# Working agreements

## Handover documents are never committed

**Never commit a handover, status or session-summary document to any
repository, and never push one to `master`.** Not under `docs/`, not at the
root, not under any name. They are working notes for the owner and belong in
the conversation, not in the history of a codebase that outlives the session.

If you find one tracked, untrack it rather than editing it.

## Merged is not fixed

**Only the owner closes a bug.** A merged PR means the change landed, nothing
more. Marking work finished because CI went green is how a list of real,
still-broken behaviour quietly empties itself.

- `shipped` when the PR merges: the honest ceiling for anything you cannot see
  with your own eyes.
- finished only after the owner says it works, or after you have driven the
  exact reported path on a running build and watched it behave.
- A passing test is not the owner saying it works. A guard can assert the
  precondition you fixed and still leave the feature broken underneath it.

When a fix cannot be verified from here, say which part is unverified rather
than rounding it up.
